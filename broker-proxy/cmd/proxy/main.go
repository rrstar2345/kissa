// Command proxy is the broker order proxy server.
//
//	proxy -config config.json     run the server
//	proxy keygen                  print a new client API key and its config hash
package main

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"syscall"
	"time"

	"brokerproxy/internal/auth"
	"brokerproxy/internal/broker"
	"brokerproxy/internal/broker/dhan"
	"brokerproxy/internal/broker/paper"
	"brokerproxy/internal/config"
	"brokerproxy/internal/server"
	"brokerproxy/internal/session"
)

func main() {
	if len(os.Args) > 1 && os.Args[1] == "keygen" {
		key, hash := auth.GenerateKey()
		fmt.Printf("API key (give to the client, shown once): %s\napiKeySha256 (put in config.json):       %s\n", key, hash)
		return
	}

	cfgPath := flag.String("config", "config.json", "path to config file")
	flag.Parse()
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, nil)))

	cfg, err := config.Load(*cfgPath)
	if err != nil {
		slog.Error("config", "err", err)
		os.Exit(1)
	}
	authn, err := auth.New(cfg.Clients)
	if err != nil {
		slog.Error("auth", "err", err)
		os.Exit(1)
	}

	// Register brokers here. A new broker is one package with an Opener.
	reg := broker.NewRegistry()
	reg.Register(paper.Name, paper.Open)
	reg.Register(dhan.Name, dhan.NewOpener(dhan.Settings{
		BaseURL: cfg.Dhan.BaseURL, FeedURL: cfg.Dhan.OrderFeedURL, OrdersPerSecond: cfg.Dhan.OrdersPerSecond,
	}))

	mgr := session.NewManager(reg, session.Options{
		IdleTimeout: cfg.IdleTimeout(), MaxLifetime: cfg.MaxLifetime(),
		MaxPerClient: cfg.Session.MaxPerClient, ReplayBuffer: cfg.Session.ReplayBuffer,
	})

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	go mgr.Run(ctx)

	s := server.New(cfg, reg, authn, mgr)
	srv := server.NewHTTPServer(cfg.ListenAddr, s.Handler())
	go func() {
		<-ctx.Done()
		slog.Info("shutting down")
		mgr.Shutdown() // drops credentials and closes client sockets first
		sctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		_ = srv.Shutdown(sctx)
	}()

	slog.Info("listening", "addr", cfg.ListenAddr, "brokers", reg.Names(), "clients", len(cfg.Clients))
	if err := s.ListenAndServe(srv); err != nil {
		slog.Error("server", "err", err)
		os.Exit(1)
	}
}
