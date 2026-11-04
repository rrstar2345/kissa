// Package auth identifies proxy clients by API key. Only SHA-256 hashes of the
// keys are configured, so a leaked config file does not reveal usable keys.
package auth

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"fmt"
	"net/http"

	"brokerproxy/internal/config"
)

const Header = "X-API-Key"

type Authenticator struct {
	byHash map[[32]byte]string
}

func New(clients []config.Client) (*Authenticator, error) {
	a := &Authenticator{byHash: map[[32]byte]string{}}
	for _, c := range clients {
		raw, err := hex.DecodeString(c.APIKeySHA256)
		if err != nil || len(raw) != 32 {
			return nil, fmt.Errorf("client %q: apiKeySha256 must be 64 hex chars", c.ID)
		}
		var h [32]byte
		copy(h[:], raw)
		a.byHash[h] = c.ID
	}
	return a, nil
}

// Client returns the client ID for the request's API key.
func (a *Authenticator) Client(r *http.Request) (string, bool) {
	k := r.Header.Get(Header)
	if k == "" {
		return "", false
	}
	id, ok := a.byHash[sha256.Sum256([]byte(k))]
	return id, ok
}

// GenerateKey returns a new random API key and the hash to put in the config.
func GenerateKey() (key, hashHex string) {
	b := make([]byte, 32)
	_, _ = rand.Read(b)
	key = base64.RawURLEncoding.EncodeToString(b)
	h := sha256.Sum256([]byte(key))
	return key, hex.EncodeToString(h[:])
}
