package session

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"log/slog"
	"net/http"
	"strings"
	"sync"
	"time"

	"brokerproxy/internal/broker"
	"brokerproxy/internal/model"
)

type Options struct {
	IdleTimeout  time.Duration
	MaxLifetime  time.Duration
	MaxPerClient int
	ReplayBuffer int
}

type Manager struct {
	reg  *broker.Registry
	opts Options

	mu       sync.Mutex
	byID     map[string]*Session
	perOwner map[string]int
}

func NewManager(reg *broker.Registry, opts Options) *Manager {
	return &Manager{reg: reg, opts: opts, byID: map[string]*Session{}, perOwner: map[string]int{}}
}

func newID() string {
	b := make([]byte, 24)
	_, _ = rand.Read(b)
	return base64.RawURLEncoding.EncodeToString(b)
}

// Create opens a broker connection with the supplied credentials and stores
// it as a new session owned by clientID.
func (m *Manager) Create(ctx context.Context, clientID, brokerName string, creds broker.Credentials) (*Session, error) {
	open, ok := m.reg.Get(brokerName)
	if !ok {
		return nil, model.BadRequest("unknown broker %q (available: %s)", brokerName, strings.Join(m.reg.Names(), ", "))
	}
	m.mu.Lock()
	if m.perOwner[clientID] >= m.opts.MaxPerClient {
		m.mu.Unlock()
		return nil, model.Errf(http.StatusTooManyRequests, model.CodeRateLimited,
			"session limit reached (%d); end an existing session first", m.opts.MaxPerClient)
	}
	m.perOwner[clientID]++ // reserve the slot while the broker handshake runs
	m.mu.Unlock()

	hub := NewHub(strings.ToLower(brokerName), m.opts.ReplayBuffer)
	conn, err := open(ctx, creds, hub.Publish)
	if err != nil {
		m.mu.Lock()
		m.perOwner[clientID]--
		m.mu.Unlock()
		return nil, err
	}
	s := &Session{
		ID: newID(), ClientID: clientID, Broker: strings.ToLower(brokerName),
		CreatedAt: time.Now(), Conn: conn, Hub: hub, idem: map[string]*idemEntry{},
	}
	s.Touch()
	m.mu.Lock()
	m.byID[s.ID] = s
	m.mu.Unlock()
	slog.Info("session opened", "session", short(s.ID), "client", clientID, "broker", s.Broker)
	return s, nil
}

// Get returns the session if it exists and belongs to clientID. Another
// client's session is reported as not found so IDs cannot be probed.
func (m *Manager) Get(id, clientID string) (*Session, error) {
	m.mu.Lock()
	s, ok := m.byID[id]
	m.mu.Unlock()
	if !ok || s.ClientID != clientID {
		return nil, model.NotFound("session not found or expired")
	}
	s.Touch()
	return s, nil
}

func (m *Manager) End(id, clientID, reason string) error {
	s, err := m.Get(id, clientID)
	if err != nil {
		return err
	}
	m.remove(s, reason)
	return nil
}

func (m *Manager) remove(s *Session, reason string) {
	m.mu.Lock()
	if _, ok := m.byID[s.ID]; !ok {
		m.mu.Unlock()
		return
	}
	delete(m.byID, s.ID)
	m.perOwner[s.ClientID]--
	m.mu.Unlock()
	s.Close(reason)
	slog.Info("session closed", "session", short(s.ID), "client", s.ClientID, "reason", reason)
}

// Run expires sessions until ctx is cancelled. A session with a WebSocket
// attached counts as active; otherwise it expires after IdleTimeout without
// REST use. Every session expires after MaxLifetime.
func (m *Manager) Run(ctx context.Context) {
	t := time.NewTicker(time.Minute)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-t.C:
			m.sweep()
		}
	}
}

func (m *Manager) sweep() {
	m.mu.Lock()
	all := make([]*Session, 0, len(m.byID))
	for _, s := range m.byID {
		all = append(all, s)
	}
	m.mu.Unlock()
	now := time.Now()
	for _, s := range all {
		if s.Hub.Subscribers() > 0 {
			s.Touch()
		}
		switch {
		case now.Sub(s.CreatedAt) > m.opts.MaxLifetime:
			m.remove(s, "session reached max lifetime")
		case now.Sub(s.LastUsed()) > m.opts.IdleTimeout:
			m.remove(s, "session idle timeout")
		}
	}
}

// Shutdown ends every session (credentials dropped, sockets notified).
func (m *Manager) Shutdown() {
	m.mu.Lock()
	all := make([]*Session, 0, len(m.byID))
	for _, s := range m.byID {
		all = append(all, s)
	}
	m.mu.Unlock()
	for _, s := range all {
		m.remove(s, "server shutting down")
	}
}

// short is a log-safe session ID prefix (the full ID is a bearer secret).
func short(id string) string {
	if len(id) > 6 {
		return id[:6]
	}
	return id
}
