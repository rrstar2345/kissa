// Package config loads the server configuration. It contains only server
// settings and proxy-client API-key hashes; broker credentials never live here.
package config

import (
	"encoding/json"
	"fmt"
	"os"
	"time"
)

type Config struct {
	ListenAddr string `json:"listenAddr"`
	TLS        struct {
		CertFile string `json:"certFile"`
		KeyFile  string `json:"keyFile"`
	} `json:"tls"`

	// Clients are the proxy's own callers, identified by API key.
	Clients []Client `json:"clients"`

	Session struct {
		IdleTimeoutMinutes int `json:"idleTimeoutMinutes"` // no REST use and no WSS attached
		MaxLifetimeHours   int `json:"maxLifetimeHours"`
		MaxPerClient       int `json:"maxPerClient"`
		ReplayBuffer       int `json:"replayBuffer"` // events kept per session for ?since= resume
	} `json:"session"`

	Dhan struct {
		BaseURL         string  `json:"baseURL"`
		OrderFeedURL    string  `json:"orderFeedURL"`
		OrdersPerSecond float64 `json:"ordersPerSecond"`
	} `json:"dhan"`
}

type Client struct {
	ID           string `json:"id"`
	APIKeySHA256 string `json:"apiKeySha256"` // hex sha256 of the API key; see `proxy keygen`
}

func Load(path string) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var c Config
	if err := json.Unmarshal(b, &c); err != nil {
		return nil, fmt.Errorf("parse %s: %w", path, err)
	}
	c.defaults()
	return &c, c.validate()
}

func (c *Config) defaults() {
	if c.ListenAddr == "" {
		c.ListenAddr = ":8443"
	}
	if c.Session.IdleTimeoutMinutes <= 0 {
		c.Session.IdleTimeoutMinutes = 60
	}
	if c.Session.MaxLifetimeHours <= 0 {
		c.Session.MaxLifetimeHours = 24
	}
	if c.Session.MaxPerClient <= 0 {
		c.Session.MaxPerClient = 10
	}
	if c.Session.ReplayBuffer <= 0 {
		c.Session.ReplayBuffer = 1000
	}
	if c.Dhan.BaseURL == "" {
		c.Dhan.BaseURL = "https://api.dhan.co/v2"
	}
	if c.Dhan.OrderFeedURL == "" {
		c.Dhan.OrderFeedURL = "wss://api-order-update.dhan.co"
	}
	if c.Dhan.OrdersPerSecond <= 0 {
		c.Dhan.OrdersPerSecond = 10
	}
}

func (c *Config) validate() error {
	if len(c.Clients) == 0 {
		return fmt.Errorf("config: at least one client is required (run `proxy keygen`)")
	}
	seen := map[string]bool{}
	for _, cl := range c.Clients {
		if cl.ID == "" || len(cl.APIKeySHA256) != 64 {
			return fmt.Errorf("config: client %q needs an id and a 64-char apiKeySha256", cl.ID)
		}
		if seen[cl.ID] {
			return fmt.Errorf("config: duplicate client id %q", cl.ID)
		}
		seen[cl.ID] = true
	}
	if (c.TLS.CertFile == "") != (c.TLS.KeyFile == "") {
		return fmt.Errorf("config: tls.certFile and tls.keyFile must be set together")
	}
	return nil
}

func (c *Config) IdleTimeout() time.Duration {
	return time.Duration(c.Session.IdleTimeoutMinutes) * time.Minute
}

func (c *Config) MaxLifetime() time.Duration {
	return time.Duration(c.Session.MaxLifetimeHours) * time.Hour
}
