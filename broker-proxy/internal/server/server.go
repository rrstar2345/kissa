// Package server exposes the REST + WebSocket API and wires it to sessions.
package server

import (
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"time"

	"brokerproxy/internal/auth"
	"brokerproxy/internal/broker"
	"brokerproxy/internal/config"
	"brokerproxy/internal/model"
	"brokerproxy/internal/session"
)

type Server struct {
	cfg  *config.Config
	reg  *broker.Registry
	auth *auth.Authenticator
	mgr  *session.Manager
}

func New(cfg *config.Config, reg *broker.Registry, a *auth.Authenticator, m *session.Manager) *Server {
	return &Server{cfg: cfg, reg: reg, auth: a, mgr: m}
}

// Handler builds the full route table.
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", s.health)

	// sessions
	mux.HandleFunc("POST /v1/sessions", s.authed(s.createSession))
	mux.HandleFunc("DELETE /v1/sessions/{id}", s.authed(s.endSession))

	// orders: same paths and bodies as Dhan v2, prefixed with /v1 and
	// scoped to the session named in the X-Session-Id header.
	mux.HandleFunc("POST /v1/orders", s.session(s.placeOrder))
	mux.HandleFunc("POST /v1/orders/slicing", s.session(s.sliceOrder))
	mux.HandleFunc("GET /v1/orders", s.session(s.listOrders))
	mux.HandleFunc("GET /v1/orders/external/{correlationId}", s.session(s.orderByCorrelation))
	mux.HandleFunc("GET /v1/orders/{orderId}", s.session(s.getOrder))
	mux.HandleFunc("PUT /v1/orders/{orderId}", s.session(s.modifyOrder))
	mux.HandleFunc("DELETE /v1/orders/{orderId}", s.session(s.cancelOrder))
	mux.HandleFunc("GET /v1/trades", s.session(s.listTrades))
	mux.HandleFunc("GET /v1/trades/{orderId}", s.session(s.tradesByOrder))

	// live order updates
	mux.HandleFunc("GET /v1/ws", s.session(s.ws))

	return recoverer(logger(mux))
}

func (s *Server) health(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{"status": "ok", "brokers": s.reg.Names()})
}

// authed requires a valid X-API-Key.
func (s *Server) authed(h func(http.ResponseWriter, *http.Request, string)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		id, ok := s.auth.Client(r)
		if !ok {
			writeErr(w, model.Errf(http.StatusUnauthorized, model.CodeUnauthorized, "missing or invalid %s", auth.Header))
			return
		}
		setClient(w, id)
		h(w, r, id)
	}
}

// session requires a valid API key and an X-Session-Id owned by that client.
func (s *Server) session(h func(http.ResponseWriter, *http.Request, *session.Session)) http.HandlerFunc {
	return s.authed(func(w http.ResponseWriter, r *http.Request, clientID string) {
		sid := r.Header.Get("X-Session-Id")
		if sid == "" {
			writeErr(w, model.BadRequest("X-Session-Id header is required (create one with POST /v1/sessions)"))
			return
		}
		sess, err := s.mgr.Get(sid, clientID)
		if err != nil {
			writeErr(w, err)
			return
		}
		h(w, r, sess)
	})
}

// ListenAndServe runs until the server is shut down.
func (s *Server) ListenAndServe(srv *http.Server) error {
	var err error
	if s.cfg.TLS.CertFile != "" {
		err = srv.ListenAndServeTLS(s.cfg.TLS.CertFile, s.cfg.TLS.KeyFile)
	} else {
		slog.Warn("TLS is not configured: credentials and API keys travel in clear text. Terminate TLS in front of this server or set tls.certFile/keyFile.")
		err = srv.ListenAndServe()
	}
	if errors.Is(err, http.ErrServerClosed) {
		return nil
	}
	return err
}

func NewHTTPServer(addr string, h http.Handler) *http.Server {
	return &http.Server{
		Addr:              addr,
		Handler:           h,
		ReadHeaderTimeout: 10 * time.Second,
		ReadTimeout:       30 * time.Second,
		IdleTimeout:       120 * time.Second,
		// No WriteTimeout: the WebSocket handler manages its own write deadlines.
	}
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}
