// pult-syncd is the sync server for Pult: it keeps a copy of Claude Code session files
// (transcripts, subagent logs, tool results, project memory, file history) sent by Pult.
//
// Files are addressed by a logical place ("root:EMBER", "home:Documents-x", "history:") and a
// path inside it, never by an absolute path of the sending machine, so machines with different
// project folders share the same sessions.
//
// Two kinds of files:
//   - log  (*.jsonl): append-only. The client sends only the bytes after the server's length;
//     an append is accepted only at exactly that offset, so a retry can never duplicate data.
//   - blob (everything else): stored whole, replaced when its sha256 changes.
//
// Storage is plain files under DATA_DIR: each stream has a meta.json, a file of concatenated zstd
// frames per generation and an index of frames. The protocol never deletes a log.
package main

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"math/big"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/klauspost/compress/zstd"
)

const (
	protocol     = 1
	tailBytes    = 4096
	maxBody      = 32 << 20  // compressed request body
	maxRawChunk  = 128 << 20 // decompressed chunk or blob
	pairTTL      = 10 * time.Minute
	pairAttempts = 5
	codeDigits   = 8
	// Wrong codes allowed per day across all pairings. With eight digits a full day of guessing
	// has a two-in-ten-million chance; after the cap pairing is closed until the window passes.
	wrongPerDay = 20
)

var version = "dev"

type Meta struct {
	Place   string `json:"place"`
	File    string `json:"file"`
	Kind    string `json:"kind"` // log | blob
	Gen     int    `json:"gen"`
	Len     int64  `json:"len"`  // raw bytes
	ZLen    int64  `json:"zlen"` // stored bytes of the current generation
	Tail    string `json:"tail,omitempty"`
	Sha     string `json:"sha,omitempty"`
	Updated int64  `json:"updated"`
	Device  string `json:"device"`
}

type Device struct {
	ID       string `json:"id"`
	Name     string `json:"name"`
	Platform string `json:"platform,omitempty"`
	TokenSha string `json:"token_sha"`
	Created  int64  `json:"created"`
	LastSeen int64  `json:"last_seen"`
}

type Place struct {
	Place   string `json:"place"`
	Base    string `json:"base"`
	Rel     string `json:"rel"`
	Device  string `json:"device"`
	Updated int64  `json:"updated"`
}

type pairing struct {
	code     string
	name     string
	platform string
	expires  time.Time
	attempts int
}

type Server struct {
	dir     string
	name    string
	minFree int64 // bytes
	tgToken string
	tgChat  string

	mu       sync.Mutex
	metas    map[string]*Meta // key: stream id
	locks    map[string]*sync.Mutex
	devices  []*Device
	devMtime time.Time
	places   map[string]*Place
	pairs    map[string]*pairing
	starts   []time.Time
	wrong    []time.Time // wrong pairing codes in the last 24 hours
}

func streamID(place, file string) string {
	sum := sha256.Sum256([]byte(place + "\x00" + file))
	return hex.EncodeToString(sum[:])
}

func (s *Server) streamDir(id string) string {
	return filepath.Join(s.dir, "streams", id[:2], id)
}

func genFile(dir string, gen int, ext string) string {
	return filepath.Join(dir, fmt.Sprintf("g%d.%s", gen, ext))
}

func writeFileAtomic(path string, data []byte, mode os.FileMode) error {
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, mode); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

func sha(b []byte) string {
	sum := sha256.Sum256(b)
	return hex.EncodeToString(sum[:])
}

func randHex(n int) string {
	b := make([]byte, n)
	if _, err := rand.Read(b); err != nil {
		panic(err)
	}
	return hex.EncodeToString(b)
}

func load(dir string) (*Server, error) {
	s := &Server{
		dir:    dir,
		metas:  map[string]*Meta{},
		locks:  map[string]*sync.Mutex{},
		places: map[string]*Place{},
		pairs:  map[string]*pairing{},
	}
	if err := os.MkdirAll(filepath.Join(dir, "streams"), 0o700); err != nil {
		return nil, err
	}
	s.loadDevices()
	if raw, err := os.ReadFile(filepath.Join(dir, "places.json")); err == nil {
		var list []*Place
		if json.Unmarshal(raw, &list) == nil {
			for _, p := range list {
				s.places[p.Place] = p
			}
		}
	}
	shards, _ := os.ReadDir(filepath.Join(dir, "streams"))
	for _, shard := range shards {
		entries, _ := os.ReadDir(filepath.Join(dir, "streams", shard.Name()))
		for _, entry := range entries {
			sdir := filepath.Join(dir, "streams", shard.Name(), entry.Name())
			raw, err := os.ReadFile(filepath.Join(sdir, "meta.json"))
			if err != nil {
				continue
			}
			var m Meta
			if json.Unmarshal(raw, &m) != nil {
				continue
			}
			// A crash between the data write and the meta write leaves extra bytes: cut them.
			if m.Kind == "log" {
				zst := genFile(sdir, m.Gen, "zst")
				if st, err := os.Stat(zst); err == nil && st.Size() > m.ZLen {
					log.Printf("recover: truncating %s/%s from %d to %d", m.Place, m.File, st.Size(), m.ZLen)
					_ = os.Truncate(zst, m.ZLen)
				}
			}
			s.metas[entry.Name()] = &m
		}
	}
	return s, nil
}

func (s *Server) devicesPath() string { return filepath.Join(s.dir, "devices.json") }

func (s *Server) loadDevices() {
	st, err := os.Stat(s.devicesPath())
	if err != nil {
		return
	}
	raw, err := os.ReadFile(s.devicesPath())
	if err != nil {
		return
	}
	var list []*Device
	if json.Unmarshal(raw, &list) == nil {
		s.devices = list
		s.devMtime = st.ModTime()
	}
}

func (s *Server) saveDevices() {
	raw, _ := json.MarshalIndent(s.devices, "", "  ")
	if err := writeFileAtomic(s.devicesPath(), raw, 0o600); err != nil {
		log.Printf("devices: %v", err)
		return
	}
	if st, err := os.Stat(s.devicesPath()); err == nil {
		s.devMtime = st.ModTime()
	}
}

func (s *Server) addDevice(name, platform string) (*Device, string) {
	token := randHex(32)
	d := &Device{ID: randHex(6), Name: name, Platform: platform, TokenSha: sha([]byte(token)), Created: time.Now().Unix()}
	s.devices = append(s.devices, d)
	s.saveDevices()
	return d, token
}

// auth returns the device for a bearer token. The devices file is re-read when it changed on disk,
// so `pult-syncd pair` works while the server runs.
func (s *Server) auth(r *http.Request) *Device {
	header := r.Header.Get("Authorization")
	token := strings.TrimPrefix(header, "Bearer ")
	if token == "" || token == header {
		return nil
	}
	want := sha([]byte(token))
	s.mu.Lock()
	defer s.mu.Unlock()
	if st, err := os.Stat(s.devicesPath()); err == nil && st.ModTime().After(s.devMtime) {
		s.loadDevices()
	}
	for _, d := range s.devices {
		if subtle.ConstantTimeCompare([]byte(d.TokenSha), []byte(want)) == 1 {
			if now := time.Now().Unix(); now-d.LastSeen > 300 {
				d.LastSeen = now
				s.saveDevices()
			}
			return d
		}
	}
	return nil
}

func (s *Server) lockStream(id string) *sync.Mutex {
	s.mu.Lock()
	l, ok := s.locks[id]
	if !ok {
		l = &sync.Mutex{}
		s.locks[id] = l
	}
	s.mu.Unlock()
	l.Lock()
	return l
}

func (s *Server) meta(id string) *Meta {
	s.mu.Lock()
	defer s.mu.Unlock()
	if m, ok := s.metas[id]; ok {
		c := *m
		return &c
	}
	return nil
}

func (s *Server) putMeta(id string, m *Meta) error {
	raw, _ := json.Marshal(m)
	if err := writeFileAtomic(filepath.Join(s.streamDir(id), "meta.json"), raw, 0o600); err != nil {
		return err
	}
	s.mu.Lock()
	c := *m
	s.metas[id] = &c
	s.mu.Unlock()
	return nil
}

func (s *Server) freeBytes() int64 {
	var st syscall.Statfs_t
	if syscall.Statfs(s.dir, &st) != nil {
		return 1 << 62
	}
	return int64(st.Bavail) * int64(st.Bsize)
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}

func fail(w http.ResponseWriter, code int, name string) {
	writeJSON(w, code, map[string]string{"error": name})
}

// "pult:" holds each computer's pinned sessions per project (projects-<device>.json).
var placeRe = regexp.MustCompile(`^(root|home|abs|history|pult):[^\x00-\x1f]{0,512}$`)

// validName accepts a place and a relative file path; they are only hashed, never used as paths,
// but garbage is refused early.
func validName(place, file string) bool {
	if !placeRe.MatchString(place) || file == "" || len(file) > 1024 {
		return false
	}
	if strings.HasPrefix(file, "/") || strings.ContainsAny(file, "\x00\n\r") {
		return false
	}
	for _, part := range strings.Split(file, "/") {
		if part == "" || part == "." || part == ".." {
			return false
		}
	}
	return true
}

func decodeZstd(body []byte) ([]byte, error) {
	dec, err := zstd.NewReader(bytes.NewReader(body), zstd.WithDecoderMaxMemory(maxRawChunk), zstd.WithDecoderConcurrency(1))
	if err != nil {
		return nil, err
	}
	defer dec.Close()
	raw, err := io.ReadAll(io.LimitReader(dec, maxRawChunk+1))
	if err != nil {
		return nil, err
	}
	if len(raw) > maxRawChunk {
		return nil, errors.New("too large")
	}
	return raw, nil
}

func readBody(w http.ResponseWriter, r *http.Request) ([]byte, bool) {
	body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, maxBody))
	if err != nil {
		fail(w, http.StatusRequestEntityTooLarge, "body_too_large")
		return nil, false
	}
	return body, true
}

func head(m *Meta) map[string]any {
	if m == nil {
		return nil
	}
	return map[string]any{"kind": m.Kind, "gen": m.Gen, "len": m.Len, "tail": m.Tail, "sha": m.Sha}
}

// appendLog adds a chunk at exactly the current end of the stream.
func (s *Server) appendLog(w http.ResponseWriter, r *http.Request, d *Device) {
	q := r.URL.Query()
	place, file := q.Get("place"), q.Get("file")
	offset, err1 := strconv.ParseInt(q.Get("offset"), 10, 64)
	gen, err2 := strconv.Atoi(q.Get("gen"))
	if !validName(place, file) || err1 != nil || err2 != nil || offset < 0 || gen < 1 {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	body, ok := readBody(w, r)
	if !ok {
		return
	}
	id := streamID(place, file)
	l := s.lockStream(id)
	defer l.Unlock()
	m := s.meta(id)
	if m != nil && m.Kind != "log" {
		fail(w, http.StatusConflict, "not_a_log")
		return
	}
	current, curGen := int64(0), 1
	if m != nil {
		current, curGen = m.Len, m.Gen
	}
	// A new generation starts from zero: the client found that its file was rewritten.
	newGen := m != nil && gen == curGen+1 && offset == 0
	if !newGen && (gen != curGen || offset != current) {
		if m == nil {
			m = &Meta{Place: place, File: file, Kind: "log", Gen: 1}
		}
		writeJSON(w, http.StatusConflict, head(m))
		return
	}
	if s.freeBytes() < s.minFree {
		fail(w, http.StatusInsufficientStorage, "disk_full")
		return
	}
	raw, err := decodeZstd(body)
	if err != nil || len(raw) == 0 {
		fail(w, http.StatusBadRequest, "bad_zstd")
		return
	}
	dir := s.streamDir(id)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	next := Meta{Place: place, File: file, Kind: "log", Gen: gen, Updated: time.Now().Unix(), Device: d.ID}
	var oldTail []byte
	if !newGen && m != nil {
		next.Len, next.ZLen = m.Len, m.ZLen
		oldTail, _ = os.ReadFile(filepath.Join(dir, "tail.bin"))
	}
	f, err := os.OpenFile(genFile(dir, gen, "zst"), os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	_, err = f.WriteAt(body, next.ZLen)
	if err == nil {
		err = f.Sync()
	}
	_ = f.Close()
	if err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	flags := os.O_CREATE | os.O_WRONLY | os.O_APPEND
	if next.Len == 0 {
		flags |= os.O_TRUNC
	}
	if idx, err := os.OpenFile(genFile(dir, gen, "idx"), flags, 0o600); err == nil {
		_, _ = fmt.Fprintf(idx, "%d %d %d %d %d %s\n", next.Len, len(raw), next.ZLen, len(body), next.Updated, d.ID)
		_ = idx.Close()
	}
	tail := append(oldTail, raw...)
	if len(tail) > tailBytes {
		tail = tail[len(tail)-tailBytes:]
	}
	_ = writeFileAtomic(filepath.Join(dir, "tail.bin"), tail, 0o600)
	next.Len += int64(len(raw))
	next.ZLen += int64(len(body))
	next.Tail = sha(tail)
	if err := s.putMeta(id, &next); err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	writeJSON(w, http.StatusOK, head(&next))
}

type frame struct{ rawOff, rawLen, zOff, zLen int64 }

func readIndex(path string) []frame {
	f, err := os.Open(path)
	if err != nil {
		return nil
	}
	defer f.Close()
	var out []frame
	sc := bufio.NewScanner(f)
	for sc.Scan() {
		var fr frame
		if n, _ := fmt.Sscanf(sc.Text(), "%d %d %d %d", &fr.rawOff, &fr.rawLen, &fr.zOff, &fr.zLen); n == 4 {
			out = append(out, fr)
		}
	}
	return out
}

// readLog streams the raw bytes [from, len) of a log as one zstd stream.
func (s *Server) readLog(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	place, file := q.Get("place"), q.Get("file")
	from, _ := strconv.ParseInt(q.Get("from"), 10, 64)
	if !validName(place, file) || from < 0 {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	id := streamID(place, file)
	l := s.lockStream(id)
	m := s.meta(id)
	if m == nil || m.Kind != "log" {
		l.Unlock()
		fail(w, http.StatusNotFound, "not_found")
		return
	}
	dir := s.streamDir(id)
	frames := readIndex(genFile(dir, m.Gen, "idx"))
	zst, err := os.Open(genFile(dir, m.Gen, "zst"))
	l.Unlock()
	if err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	defer zst.Close()
	w.Header().Set("Content-Type", "application/zstd")
	w.Header().Set("X-Pult-Len", strconv.FormatInt(m.Len, 10))
	w.Header().Set("X-Pult-Gen", strconv.Itoa(m.Gen))
	if from >= m.Len {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	enc, err := zstd.NewWriter(w, zstd.WithEncoderConcurrency(1))
	if err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	defer enc.Close()
	// The index may hold a frame from an append that crashed before its meta was written and
	// was then repeated: a later line for the same offset wins.
	seen := map[int64]frame{}
	var order []int64
	for _, fr := range frames {
		if fr.zOff+fr.zLen > m.ZLen {
			continue
		}
		if _, dup := seen[fr.rawOff]; !dup {
			order = append(order, fr.rawOff)
		}
		seen[fr.rawOff] = fr
	}
	for _, off := range order {
		fr := seen[off]
		if fr.rawOff+fr.rawLen <= from {
			continue
		}
		body := make([]byte, fr.zLen)
		if _, err := zst.ReadAt(body, fr.zOff); err != nil {
			return
		}
		raw, err := decodeZstd(body)
		if err != nil {
			return
		}
		if from > fr.rawOff {
			raw = raw[from-fr.rawOff:]
		}
		if _, err := enc.Write(raw); err != nil {
			return
		}
	}
}

func (s *Server) putBlob(w http.ResponseWriter, r *http.Request, d *Device) {
	q := r.URL.Query()
	place, file, want := q.Get("place"), q.Get("file"), q.Get("sha256")
	if !validName(place, file) || len(want) != 64 {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	body, ok := readBody(w, r)
	if !ok {
		return
	}
	id := streamID(place, file)
	l := s.lockStream(id)
	defer l.Unlock()
	m := s.meta(id)
	if m != nil && m.Kind != "blob" {
		fail(w, http.StatusConflict, "not_a_blob")
		return
	}
	if m != nil && m.Sha == want {
		writeJSON(w, http.StatusOK, head(m))
		return
	}
	if s.freeBytes() < s.minFree {
		fail(w, http.StatusInsufficientStorage, "disk_full")
		return
	}
	raw, err := decodeZstd(body)
	if err != nil || sha(raw) != want {
		fail(w, http.StatusBadRequest, "bad_content")
		return
	}
	dir := s.streamDir(id)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	next := Meta{Place: place, File: file, Kind: "blob", Gen: 1, Len: int64(len(raw)), ZLen: int64(len(body)), Sha: want,
		Updated: time.Now().Unix(), Device: d.ID}
	if m != nil {
		next.Gen = m.Gen + 1
	}
	if err := writeFileAtomic(genFile(dir, next.Gen, "zst"), body, 0o600); err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	if err := s.putMeta(id, &next); err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	// Only the previous version of a blob is kept.
	if next.Gen > 2 {
		_ = os.Remove(genFile(dir, next.Gen-2, "zst"))
	}
	writeJSON(w, http.StatusOK, head(&next))
}

func (s *Server) getBlob(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	place, file := q.Get("place"), q.Get("file")
	if !validName(place, file) {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	id := streamID(place, file)
	m := s.meta(id)
	if m == nil || m.Kind != "blob" {
		fail(w, http.StatusNotFound, "not_found")
		return
	}
	body, err := os.ReadFile(genFile(s.streamDir(id), m.Gen, "zst"))
	if err != nil {
		fail(w, http.StatusInternalServerError, "storage")
		return
	}
	w.Header().Set("Content-Type", "application/zstd")
	w.Header().Set("X-Pult-Sha256", m.Sha)
	_, _ = w.Write(body)
}

func (s *Server) heads(w http.ResponseWriter, r *http.Request) {
	body, ok := readBody(w, r)
	if !ok {
		return
	}
	var req struct {
		Items []struct {
			Place string `json:"place"`
			File  string `json:"file"`
		} `json:"items"`
	}
	if json.Unmarshal(body, &req) != nil || len(req.Items) > 5000 {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	out := make([]map[string]any, len(req.Items))
	for i, it := range req.Items {
		out[i] = head(s.meta(streamID(it.Place, it.File)))
	}
	writeJSON(w, http.StatusOK, map[string]any{"items": out})
}

var sessionRe = regexp.MustCompile(`^([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$`)

func (s *Server) sessions(w http.ResponseWriter) {
	type row struct {
		Session string `json:"session"`
		Place   string `json:"place"`
		Rel     string `json:"rel,omitempty"`
		Len     int64  `json:"len"`
		Updated int64  `json:"updated"`
		Device  string `json:"device"`
	}
	rows := []row{}
	s.mu.Lock()
	for _, m := range s.metas {
		if match := sessionRe.FindStringSubmatch(m.File); match != nil && m.Kind == "log" {
			rw := row{Session: match[1], Place: m.Place, Len: m.Len, Updated: m.Updated, Device: m.Device}
			if p, ok := s.places[m.Place]; ok {
				rw.Rel = p.Rel
			}
			rows = append(rows, rw)
		}
	}
	s.mu.Unlock()
	sort.Slice(rows, func(i, j int) bool { return rows[i].Updated > rows[j].Updated })
	writeJSON(w, http.StatusOK, map[string]any{"sessions": rows})
}

// Updates: the app's installers live in DATA_DIR/updates; latest.json describes the newest one.
// `pult-syncd publish <dmg> <version> [notes.txt]` puts a new one there (one note per line).
type Release struct {
	Version   string   `json:"version"`
	SHA256    string   `json:"sha256"`
	Size      int64    `json:"size"`
	Notes     []string `json:"notes"`
	File      string   `json:"file"`
	Published int64    `json:"published"`
}

func (s *Server) updatesDir() string { return filepath.Join(s.dir, "updates") }

func (s *Server) latestRelease() (*Release, error) {
	raw, err := os.ReadFile(filepath.Join(s.updatesDir(), "latest.json"))
	if err != nil {
		return nil, err
	}
	var rel Release
	if err := json.Unmarshal(raw, &rel); err != nil {
		return nil, err
	}
	return &rel, nil
}

func (s *Server) updateInfo(w http.ResponseWriter) {
	rel, err := s.latestRelease()
	if err != nil {
		fail(w, http.StatusNotFound, "not_found")
		return
	}
	writeJSON(w, http.StatusOK, rel)
}

func (s *Server) updateDownload(w http.ResponseWriter, r *http.Request) {
	rel, err := s.latestRelease()
	if err != nil || rel.File == "" || strings.ContainsAny(rel.File, "/\\") {
		fail(w, http.StatusNotFound, "not_found")
		return
	}
	f, err := os.Open(filepath.Join(s.updatesDir(), rel.File))
	if err != nil {
		fail(w, http.StatusNotFound, "not_found")
		return
	}
	defer f.Close()
	w.Header().Set("Content-Type", "application/x-apple-diskimage")
	w.Header().Set("X-Pult-Sha256", rel.SHA256)
	http.ServeContent(w, r, rel.File, time.Unix(rel.Published, 0), f)
}

// publish copies an installer into the updates folder and makes it the latest release.
func (s *Server) publish(dmg, version, notesFile string) error {
	if !regexp.MustCompile(`^[0-9]+(\.[0-9]+){1,3}$`).MatchString(version) {
		return fmt.Errorf("bad version %q", version)
	}
	src, err := os.Open(dmg)
	if err != nil {
		return err
	}
	defer src.Close()
	if err := os.MkdirAll(s.updatesDir(), 0o700); err != nil {
		return err
	}
	name := "Pult-" + version + ".dmg"
	tmp := filepath.Join(s.updatesDir(), name+".tmp")
	dst, err := os.OpenFile(tmp, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	h := sha256.New()
	size, err := io.Copy(io.MultiWriter(dst, h), src)
	if cerr := dst.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		os.Remove(tmp)
		return err
	}
	if err := os.Rename(tmp, filepath.Join(s.updatesDir(), name)); err != nil {
		return err
	}
	var notes []string
	if notesFile != "" {
		raw, err := os.ReadFile(notesFile)
		if err != nil {
			return err
		}
		for _, line := range strings.Split(string(raw), "\n") {
			if line = strings.TrimSpace(line); line != "" {
				notes = append(notes, line)
			}
		}
	}
	rel := Release{Version: version, SHA256: hex.EncodeToString(h.Sum(nil)), Size: size, Notes: notes, File: name, Published: time.Now().Unix()}
	raw, _ := json.MarshalIndent(rel, "", " ")
	if err := writeFileAtomic(filepath.Join(s.updatesDir(), "latest.json"), raw, 0o600); err != nil {
		return err
	}
	// Keep the three newest installers.
	entries, _ := os.ReadDir(s.updatesDir())
	var dmgs []os.DirEntry
	for _, e := range entries {
		if strings.HasSuffix(e.Name(), ".dmg") {
			dmgs = append(dmgs, e)
		}
	}
	sort.Slice(dmgs, func(i, j int) bool {
		a, _ := dmgs[i].Info()
		b, _ := dmgs[j].Info()
		return a.ModTime().After(b.ModTime())
	})
	for i, e := range dmgs {
		if i >= 3 && e.Name() != name {
			os.Remove(filepath.Join(s.updatesDir(), e.Name()))
		}
	}
	fmt.Printf("published %s: %d bytes, sha256 %s, %d notes\n", version, size, rel.SHA256, len(notes))
	return nil
}

// files lists everything the server keeps, for a computer that fetches what it does not have yet.
// With ?since=<unix> only files changed at or after that time are listed; "now" is the value to pass
// next time.
func (s *Server) files(w http.ResponseWriter, r *http.Request) {
	since, _ := strconv.ParseInt(r.URL.Query().Get("since"), 10, 64)
	type row struct {
		Place   string `json:"place"`
		File    string `json:"file"`
		Kind    string `json:"kind"`
		Gen     int    `json:"gen"`
		Len     int64  `json:"len"`
		Tail    string `json:"tail,omitempty"`
		Sha     string `json:"sha,omitempty"`
		Updated int64  `json:"updated"`
		Device  string `json:"device"`
	}
	now := time.Now().Unix()
	rows := []row{}
	s.mu.Lock()
	for _, m := range s.metas {
		if m.Updated < since {
			continue
		}
		rows = append(rows, row{m.Place, m.File, m.Kind, m.Gen, m.Len, m.Tail, m.Sha, m.Updated, m.Device})
	}
	s.mu.Unlock()
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].Place != rows[j].Place {
			return rows[i].Place < rows[j].Place
		}
		return rows[i].File < rows[j].File
	})
	writeJSON(w, http.StatusOK, map[string]any{"files": rows, "now": now})
}

func (s *Server) putPlace(w http.ResponseWriter, r *http.Request, d *Device) {
	body, ok := readBody(w, r)
	if !ok {
		return
	}
	var p Place
	if json.Unmarshal(body, &p) != nil || !placeRe.MatchString(p.Place) || len(p.Rel) > 1024 {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	p.Device, p.Updated = d.ID, time.Now().Unix()
	s.mu.Lock()
	old, had := s.places[p.Place]
	if !had || old.Rel != p.Rel || old.Base != p.Base {
		s.places[p.Place] = &p
		list := make([]*Place, 0, len(s.places))
		for _, v := range s.places {
			list = append(list, v)
		}
		sort.Slice(list, func(i, j int) bool { return list[i].Place < list[j].Place })
		raw, _ := json.MarshalIndent(list, "", " ")
		_ = writeFileAtomic(filepath.Join(s.dir, "places.json"), raw, 0o600)
	}
	s.mu.Unlock()
	writeJSON(w, http.StatusOK, map[string]bool{"ok": true})
}

func (s *Server) status(w http.ResponseWriter, d *Device) {
	var streams int
	var raw, stored int64
	s.mu.Lock()
	for _, m := range s.metas {
		streams++
		raw += m.Len
		stored += m.ZLen
	}
	devices := len(s.devices)
	s.mu.Unlock()
	writeJSON(w, http.StatusOK, map[string]any{
		"server": s.name, "version": version, "protocol": protocol, "device": d.ID, "device_name": d.Name,
		"streams": streams, "raw_bytes": raw, "stored_bytes": stored, "free_bytes": s.freeBytes(), "devices": devices,
	})
}

func (s *Server) pairingVia() string {
	if s.tgToken != "" {
		return "telegram"
	}
	return "console"
}

func (s *Server) discovery(w http.ResponseWriter) {
	writeJSON(w, http.StatusOK, map[string]any{
		"service": "pult-sync", "protocol": protocol, "api": "/sync/v1", "name": s.name, "version": version,
		"pairing": s.pairingVia(),
	})
}

// notify tells the administrator something out of band: Telegram when configured, the service log otherwise.
func (s *Server) notify(text string) {
	if s.tgToken == "" {
		log.Printf("notice: %s", text)
		return
	}
	form := url.Values{"chat_id": {s.tgChat}, "text": {text}}
	client := &http.Client{Timeout: 15 * time.Second}
	resp, err := client.PostForm("https://api.telegram.org/bot"+s.tgToken+"/sendMessage", form)
	if err != nil {
		log.Printf("telegram: request failed")
		return
	}
	_ = resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		log.Printf("telegram: status %d", resp.StatusCode)
	}
}

// pairingLocked reports whether too many wrong codes were tried in the last day. Call with s.mu held.
func (s *Server) pairingLocked(now time.Time) bool {
	recent := s.wrong[:0]
	for _, t := range s.wrong {
		if now.Sub(t) < 24*time.Hour {
			recent = append(recent, t)
		}
	}
	s.wrong = recent
	return len(s.wrong) >= wrongPerDay
}

func (s *Server) pairStart(w http.ResponseWriter, r *http.Request) {
	body, ok := readBody(w, r)
	if !ok {
		return
	}
	var req struct {
		Name     string `json:"name"`
		Platform string `json:"platform"`
	}
	if json.Unmarshal(body, &req) != nil || strings.TrimSpace(req.Name) == "" || len(req.Name) > 80 {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	now := time.Now()
	s.mu.Lock()
	recent := s.starts[:0]
	for _, t := range s.starts {
		if now.Sub(t) < pairTTL {
			recent = append(recent, t)
		}
	}
	s.starts = recent
	if s.pairingLocked(now) {
		s.mu.Unlock()
		fail(w, http.StatusTooManyRequests, "pairing_locked")
		return
	}
	if len(s.starts) >= 5 {
		s.mu.Unlock()
		fail(w, http.StatusTooManyRequests, "too_many_requests")
		return
	}
	s.starts = append(s.starts, now)
	for id, p := range s.pairs {
		if now.After(p.expires) {
			delete(s.pairs, id)
		}
	}
	limit := big.NewInt(1)
	for i := 0; i < codeDigits; i++ {
		limit.Mul(limit, big.NewInt(10))
	}
	n, _ := rand.Int(rand.Reader, limit)
	id, code := randHex(12), fmt.Sprintf("%0*d", codeDigits, n.Int64())
	name := strings.TrimSpace(req.Name)
	s.pairs[id] = &pairing{code: code, name: name, platform: req.Platform, expires: now.Add(pairTTL)}
	s.mu.Unlock()
	go s.notify(fmt.Sprintf("Pult: “%s” is connecting. Code: %s (valid for 10 minutes). If this isn't you, don't share it.", name, code))
	writeJSON(w, http.StatusOK, map[string]any{"pairing": id, "expires_in": int(pairTTL.Seconds()), "via": s.pairingVia(), "digits": codeDigits})
}

func (s *Server) pairFinish(w http.ResponseWriter, r *http.Request) {
	body, ok := readBody(w, r)
	if !ok {
		return
	}
	var req struct {
		Pairing string `json:"pairing"`
		Code    string `json:"code"`
	}
	if json.Unmarshal(body, &req) != nil {
		fail(w, http.StatusBadRequest, "bad_request")
		return
	}
	s.mu.Lock()
	if s.pairingLocked(time.Now()) {
		s.mu.Unlock()
		fail(w, http.StatusTooManyRequests, "pairing_locked")
		return
	}
	p, found := s.pairs[req.Pairing]
	if !found || time.Now().After(p.expires) {
		delete(s.pairs, req.Pairing)
		s.mu.Unlock()
		fail(w, http.StatusGone, "pairing_expired")
		return
	}
	if subtle.ConstantTimeCompare([]byte(p.code), []byte(strings.TrimSpace(req.Code))) != 1 {
		p.attempts++
		if p.attempts >= pairAttempts {
			delete(s.pairs, req.Pairing)
		}
		s.wrong = append(s.wrong, time.Now())
		locked := len(s.wrong) == wrongPerDay
		s.mu.Unlock()
		if locked {
			go s.notify("Pult: too many wrong connection codes. New computers can't connect for a day; connected ones keep working.")
		}
		fail(w, http.StatusForbidden, "wrong_code")
		return
	}
	delete(s.pairs, req.Pairing)
	d, token := s.addDevice(p.name, p.platform)
	s.mu.Unlock()
	go s.notify(fmt.Sprintf("Pult: “%s” is now connected to sync.", d.Name))
	writeJSON(w, http.StatusOK, map[string]any{"device": d.ID, "token": token, "server": s.name})
}

func (s *Server) revoke(w http.ResponseWriter, d *Device) {
	s.mu.Lock()
	kept := s.devices[:0]
	for _, x := range s.devices {
		if x.ID != d.ID {
			kept = append(kept, x)
		}
	}
	s.devices = kept
	s.saveDevices()
	s.mu.Unlock()
	writeJSON(w, http.StatusOK, map[string]bool{"ok": true})
}

func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	path := strings.TrimSuffix(r.URL.Path, "/")
	if path == "/.well-known/pult-sync" || path == "/sync/v1" {
		s.discovery(w)
		return
	}
	if !strings.HasPrefix(path, "/sync/v1/") {
		fail(w, http.StatusNotFound, "not_found")
		return
	}
	route := r.Method + " " + strings.TrimPrefix(path, "/sync/v1")
	switch route {
	case "POST /pair/start":
		s.pairStart(w, r)
		return
	case "POST /pair/finish":
		s.pairFinish(w, r)
		return
	// Updates are open: a computer without sync gets them too. The installer is signed and notarized, and Pult
	// checks the signature and the checksum before installing.
	case "GET /update":
		s.updateInfo(w)
		return
	case "GET /update/download":
		s.updateDownload(w, r)
		return
	}
	d := s.auth(r)
	if d == nil {
		fail(w, http.StatusUnauthorized, "unauthorized")
		return
	}
	switch route {
	case "GET /me", "GET /status":
		s.status(w, d)
	case "POST /heads":
		s.heads(w, r)
	case "PUT /log":
		s.appendLog(w, r, d)
	case "GET /log":
		s.readLog(w, r)
	case "PUT /blob":
		s.putBlob(w, r, d)
	case "GET /blob":
		s.getBlob(w, r)
	case "GET /sessions":
		s.sessions(w)
	case "GET /files":
		s.files(w, r)
	case "PUT /place":
		s.putPlace(w, r, d)
	case "DELETE /device":
		s.revoke(w, d)
	default:
		fail(w, http.StatusNotFound, "not_found")
	}
}

func env(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func main() {
	dir := env("DATA_DIR", "./data")
	s, err := load(dir)
	if err != nil {
		log.Fatalf("data dir: %v", err)
	}
	s.name = env("SERVER_NAME", "pult-sync")
	minFree, _ := strconv.ParseInt(env("MIN_FREE_MB", "1500"), 10, 64)
	s.minFree = minFree << 20
	s.tgToken, s.tgChat = os.Getenv("TELEGRAM_BOT_TOKEN"), os.Getenv("TELEGRAM_CHAT_ID")

	args := os.Args[1:]
	switch {
	case len(args) >= 2 && args[0] == "pair":
		// Direct pairing from the server's console: prints the token once.
		d, token := s.addDevice(strings.Join(args[1:], " "), "")
		fmt.Printf("device %s (%s)\ntoken %s\n", d.ID, d.Name, token)
		return
	case len(args) == 1 && args[0] == "devices":
		for _, d := range s.devices {
			fmt.Printf("%s\t%s\t%s\tlast seen %s\n", d.ID, d.Name, d.Platform, time.Unix(d.LastSeen, 0).Format(time.RFC3339))
		}
		return
	case len(args) >= 3 && args[0] == "publish":
		notes := ""
		if len(args) >= 4 {
			notes = args[3]
		}
		if err := s.publish(args[1], args[2], notes); err != nil {
			log.Fatalf("publish: %v", err)
		}
		return
	case len(args) == 1 && args[0] == "version":
		fmt.Println(version)
		return
	case len(args) > 0:
		log.Fatalf("usage: pult-syncd [pair <name> | devices | publish <dmg> <version> [notes.txt] | version]")
	}
	addr := env("LISTEN", "127.0.0.1:8647")
	srv := &http.Server{Addr: addr, Handler: s, ReadHeaderTimeout: 15 * time.Second, IdleTimeout: 120 * time.Second}
	log.Printf("pult-syncd %s: %s, data %s, %d streams, %d devices", version, addr, dir, len(s.metas), len(s.devices))
	log.Fatal(srv.ListenAndServe())
}
