// Package broker defines the contract every broker adapter implements and the
// registry the session layer uses to open connections. Adding a broker means
// writing one package that exposes an Open function and registering it in main.
package broker

import (
	"context"
	"sort"
	"strings"

	"brokerproxy/internal/model"
)

// Credentials are supplied by the client when it opens a session. They are
// held only in memory for the life of the session and never written anywhere.
type Credentials map[string]string

// Sink receives live events (order updates, feed status) from a Conn.
// It must not block; the session hub guarantees that.
type Sink func(model.Event)

// Conn is one authenticated broker account bound to one session. Inputs are
// already validated. Implementations must be safe for concurrent use.
type Conn interface {
	Name() string

	PlaceOrder(ctx context.Context, o *model.OrderRequest) (*model.OrderAck, error)
	ModifyOrder(ctx context.Context, orderID string, m *model.ModifyRequest) (*model.OrderAck, error)
	CancelOrder(ctx context.Context, orderID string) (*model.OrderAck, error)
	SliceOrder(ctx context.Context, o *model.OrderRequest) ([]model.OrderAck, error)

	Orders(ctx context.Context) ([]model.Order, error)
	Order(ctx context.Context, orderID string) (*model.Order, error)
	OrderByCorrelationID(ctx context.Context, correlationID string) (*model.Order, error)
	Trades(ctx context.Context) ([]model.Trade, error)
	TradesByOrder(ctx context.Context, orderID string) ([]model.Trade, error)

	// Close stops any live feed and drops the credentials.
	Close() error
}

// Opener validates the credentials, starts the broker's live feed (if any,
// delivering events to sink) and returns the Conn.
type Opener func(ctx context.Context, creds Credentials, sink Sink) (Conn, error)

// Registry maps broker names to openers.
type Registry struct{ m map[string]Opener }

func NewRegistry() *Registry { return &Registry{m: map[string]Opener{}} }

func (r *Registry) Register(name string, o Opener) { r.m[strings.ToLower(name)] = o }

func (r *Registry) Get(name string) (Opener, bool) {
	o, ok := r.m[strings.ToLower(name)]
	return o, ok
}

func (r *Registry) Names() []string {
	n := make([]string, 0, len(r.m))
	for k := range r.m {
		n = append(n, k)
	}
	sort.Strings(n)
	return n
}
