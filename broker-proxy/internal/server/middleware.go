package server

import (
	"bufio"
	"errors"
	"log/slog"
	"net"
	"net/http"
	"runtime/debug"
	"time"
)

// statusWriter records the response status for logging and keeps the
// Hijacker interface so WebSocket upgrades keep working.
type statusWriter struct {
	http.ResponseWriter
	status int
	client string
}

func (w *statusWriter) WriteHeader(code int) {
	w.status = code
	w.ResponseWriter.WriteHeader(code)
}

func (w *statusWriter) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	h, ok := w.ResponseWriter.(http.Hijacker)
	if !ok {
		return nil, nil, errors.New("hijack unsupported")
	}
	w.status = http.StatusSwitchingProtocols
	return h.Hijack()
}

func (w *statusWriter) Flush() {
	if f, ok := w.ResponseWriter.(http.Flusher); ok {
		f.Flush()
	}
}

// setClient lets the auth layer tag the access log line with the client ID.
func setClient(w http.ResponseWriter, id string) {
	if sw, ok := w.(*statusWriter); ok {
		sw.client = id
	}
}

// logger writes one access-log line per request. It never logs headers,
// query strings or bodies, so API keys, session IDs and broker credentials
// cannot leak into logs.
func logger(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		start := time.Now()
		sw := &statusWriter{ResponseWriter: w, status: http.StatusOK}
		next.ServeHTTP(sw, r)
		slog.Info("request", "method", r.Method, "path", r.URL.Path, "status", sw.status,
			"client", sw.client, "ms", time.Since(start).Milliseconds())
	})
}

func recoverer(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer func() {
			if v := recover(); v != nil {
				slog.Error("panic", "path", r.URL.Path, "panic", v, "stack", string(debug.Stack()))
				writeErr(w, nil)
			}
		}()
		next.ServeHTTP(w, r)
	})
}
