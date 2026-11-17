// Package main serves the ledger and its reconciliation over HTTP.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

const (
	defaultAddr     = ":8080"
	readTimeout     = 10 * time.Second
	writeTimeout    = 30 * time.Second
	shutdownTimeout = 5 * time.Second
	maxPageSize     = 100
	maxBodyBytes    = 1 << 20
)

// Config is what the server reads from its environment.
type Config struct {
	Addr     string
	Token    string
	PageSize int
	Verbose  bool
	ReadOnly bool
}

// loadConfig reads the configuration from the environment, with defaults.
func loadConfig() Config {
	config := Config{
		Addr:     defaultAddr,
		Token:    os.Getenv("LEDGER_TOKEN"),
		PageSize: 50,
	}
	if addr := os.Getenv("LEDGER_ADDR"); addr != "" {
		config.Addr = addr
	}
	if size, err := strconv.Atoi(os.Getenv("LEDGER_PAGE_SIZE")); err == nil && size > 0 {
		config.PageSize = min(size, maxPageSize)
	}
	config.Verbose = os.Getenv("LEDGER_VERBOSE") != ""
	config.ReadOnly = os.Getenv("LEDGER_READ_ONLY") != ""
	return config
}

// Entry is one booking: an amount in cents on an account.
type Entry struct {
	Account string `json:"account"`
	Cents   int64  `json:"cents"`
	Memo    string `json:"memo,omitempty"`
	Ref     string `json:"ref,omitempty"`
}

// Ledger keeps the balances of every account.
type Ledger struct {
	mu       sync.RWMutex
	balances map[string]int64
	applied  int
}

// NewLedger returns an empty ledger.
func NewLedger() *Ledger {
	return &Ledger{balances: make(map[string]int64)}
}

// Apply books one entry.
func (l *Ledger) Apply(entry Entry) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.balances[entry.Account] += entry.Cents
	l.applied++
}

// Balance returns the balance of one account.
func (l *Ledger) Balance(account string) int64 {
	l.mu.RLock()
	defer l.mu.RUnlock()
	return l.balances[account]
}

// Accounts returns every account name, sorted.
func (l *Ledger) Accounts() []string {
	l.mu.RLock()
	defer l.mu.RUnlock()
	names := make([]string, 0, len(l.balances))
	for name := range l.balances {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// Server answers the ledger's HTTP API.
type Server struct {
	config Config
	ledger *Ledger
	logger *log.Logger
}

// NewServer returns a server over ledger.
func NewServer(config Config, ledger *Ledger) *Server {
	return &Server{
		config: config,
		ledger: ledger,
		logger: log.New(os.Stderr, "ledger ", log.LstdFlags),
	}
}

// routes registers every handler.
func (s *Server) routes() *http.ServeMux {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /health", s.handleHealth)
	mux.HandleFunc("GET /accounts", s.handleAccounts)
	mux.HandleFunc("GET /accounts/{name}/balance", s.handleBalance)
	mux.HandleFunc("POST /entries", s.handleEntries)
	mux.HandleFunc("POST /reconcile", s.handleReconcile)
	return mux
}

// withLogging logs every request and how long it took.
func (s *Server) withLogging(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		started := time.Now()
		next.ServeHTTP(w, r)
		if s.config.Verbose {
			s.logger.Printf("%s %s in %s", r.Method, r.URL.Path, time.Since(started).Round(time.Millisecond))
		}
	})
}

// withAuth refuses requests without the configured token.
func (s *Server) withAuth(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if s.config.Token == "" {
			next.ServeHTTP(w, r)
			return
		}
		token := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
		if token != s.config.Token {
			writeError(w, http.StatusUnauthorized, "a valid bearer token is required")
			return
		}
		next.ServeHTTP(w, r)
	})
}

// handleHealth answers that the server is up.
func (s *Server) handleHealth(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{"status": "ok", "read_only": s.config.ReadOnly})
}

// handleAccounts lists the accounts, a page at a time.
func (s *Server) handleAccounts(w http.ResponseWriter, r *http.Request) {
	names := s.ledger.Accounts()
	page, err := strconv.Atoi(r.URL.Query().Get("page"))
	if err != nil || page < 1 {
		page = 1
	}
	start := min((page-1)*s.config.PageSize, len(names))
	end := min(start+s.config.PageSize, len(names))
	writeJSON(w, http.StatusOK, map[string]any{
		"accounts": names[start:end],
		"page":     page,
		"total":    len(names),
	})
}

// handleBalance answers one account's balance.
func (s *Server) handleBalance(w http.ResponseWriter, r *http.Request) {
	name := r.PathValue("name")
	if name == "" {
		writeError(w, http.StatusBadRequest, "the account name is missing")
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"account": name,
		"cents":   s.ledger.Balance(name),
	})
}

// handleEntries books the entries in the request body.
func (s *Server) handleEntries(w http.ResponseWriter, r *http.Request) {
	if s.config.ReadOnly {
		writeError(w, http.StatusForbidden, "the ledger is read-only")
		return
	}
	var entries []Entry
	body := http.MaxBytesReader(w, r.Body, maxBodyBytes)
	if err := json.NewDecoder(body).Decode(&entries); err != nil {
		writeError(w, http.StatusBadRequest, "invalid entries")
		return
	}
	for _, entry := range entries {
		if entry.Account == "" {
			writeError(w, http.StatusBadRequest, "an entry has no account")
			return
		}
	}
	for _, entry := range entries {
		s.ledger.Apply(entry)
	}
	writeJSON(w, http.StatusCreated, map[string]int{"applied": len(entries)})
}

// Mismatch is an account whose balance differs from the expected one.
type Mismatch struct {
	Account  string `json:"account"`
	Expected int64  `json:"expected"`
	Actual   int64  `json:"actual"`
}

// handleReconcile compares the balances in the request body with the ledger's and answers
// every account that differs, sorted by account, with both amounts.
func (s *Server) handleReconcile(w http.ResponseWriter, r *http.Request) {
	var expected map[string]int64
	body := http.MaxBytesReader(w, r.Body, maxBodyBytes)
	if err := json.NewDecoder(body).Decode(&expected); err != nil {
		var tooLarge *http.MaxBytesError
		if errors.As(err, &tooLarge) {
			writeError(w, http.StatusRequestEntityTooLarge, "the balances are too large")
			return
		}
		writeError(w, http.StatusBadRequest, "invalid balances")
		return
	}
	accounts := make([]string, 0, len(expected))
	for account := range expected {
		accounts = append(accounts, account)
	}
	for _, account := range s.ledger.Accounts() {
		if _, listed := expected[account]; !listed {
			accounts = append(accounts, account)
		}
	}
	sort.Strings(accounts)
	mismatches := make([]Mismatch, 0)
	for _, account := range accounts {
		actual := s.ledger.Balance(account)
		if actual == expected[account] {
			continue
		}
		mismatches = append(mismatches, Mismatch{
			Account:  account,
			Expected: expected[account],
			Actual:   actual,
		})
	}
	status := http.StatusOK
	if len(mismatches) > 0 {
		status = http.StatusConflict
	}
	writeJSON(w, status, map[string]any{
		"checked":    len(accounts),
		"mismatches": mismatches,
	})
}

// writeJSON answers with value as JSON.
func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json; charset=utf-8")
	w.WriteHeader(status)
	if err := json.NewEncoder(w).Encode(value); err != nil {
		log.Printf("writing a response: %v", err)
	}
}

// writeError answers with an error message.
func writeError(w http.ResponseWriter, status int, message string) {
	writeJSON(w, status, map[string]string{"error": message})
}

// run serves until the process is asked to stop, then shuts down gracefully.
func run(config Config) error {
	server := NewServer(config, NewLedger())
	handler := server.withLogging(server.withAuth(server.routes()))
	httpServer := &http.Server{
		Addr:         config.Addr,
		Handler:      handler,
		ReadTimeout:  readTimeout,
		WriteTimeout: writeTimeout,
	}
	stopped := make(chan os.Signal, 1)
	signal.Notify(stopped, os.Interrupt, syscall.SIGTERM)
	failed := make(chan error, 1)
	go func() {
		server.logger.Printf("listening on %s (read-only: %t)", config.Addr, config.ReadOnly)
		failed <- httpServer.ListenAndServe()
	}()
	select {
	case err := <-failed:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return err
	case <-stopped:
	}
	ctx, cancel := context.WithTimeout(context.Background(), shutdownTimeout)
	defer cancel()
	return httpServer.Shutdown(ctx)
}

func main() {
	if err := run(loadConfig()); err != nil {
		fmt.Fprintln(os.Stderr, "ledger server:", err)
		os.Exit(2)
	}
}
