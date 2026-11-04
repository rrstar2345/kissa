package dhan

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"brokerproxy/internal/model"
)

// limiter spaces requests evenly at the configured rate (Dhan limits order
// APIs per second). Waits longer than maxWait are refused rather than queued.
type limiter struct {
	mu       sync.Mutex
	interval time.Duration
	next     time.Time
}

const maxWait = 2 * time.Second

func newLimiter(perSecond float64) *limiter {
	return &limiter{interval: time.Duration(float64(time.Second) / perSecond)}
}

func (l *limiter) Wait(ctx context.Context) error {
	l.mu.Lock()
	now := time.Now()
	slot := l.next
	if slot.Before(now) {
		slot = now
	}
	wait := slot.Sub(now)
	if wait > maxWait {
		l.mu.Unlock()
		return model.Errf(http.StatusTooManyRequests, model.CodeRateLimited, "order rate limit exceeded; retry shortly")
	}
	l.next = slot.Add(l.interval)
	l.mu.Unlock()
	if wait == 0 {
		return nil
	}
	t := time.NewTimer(wait)
	defer t.Stop()
	select {
	case <-t.C:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

// client is a thin authenticated REST client for one Dhan account.
type client struct {
	http     *http.Client
	base     string
	clientID string
	token    string
	lim      *limiter
}

// do performs a request. write=true applies the order rate limiter.
func (c *client) do(ctx context.Context, method, path string, in, out any, write bool) error {
	if write {
		if err := c.lim.Wait(ctx); err != nil {
			return err
		}
	}
	var body io.Reader
	if in != nil {
		b, err := json.Marshal(in)
		if err != nil {
			return model.Errf(http.StatusInternalServerError, model.CodeInternal, "encode request: %v", err)
		}
		body = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, c.base+path, body)
	if err != nil {
		return model.Errf(http.StatusInternalServerError, model.CodeInternal, "build request: %v", err)
	}
	req.Header.Set("access-token", c.token)
	req.Header.Set("Accept", "application/json")
	if in != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.http.Do(req)
	if err != nil {
		if ctx.Err() != nil {
			return ctx.Err()
		}
		return model.Errf(http.StatusBadGateway, model.CodeUpstream, "dhan request failed: %v", err)
	}
	defer resp.Body.Close()
	data, _ := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	if resp.StatusCode/100 != 2 {
		return mapHTTPError(resp.StatusCode, data)
	}
	if out != nil && len(bytes.TrimSpace(data)) > 0 {
		if err := json.Unmarshal(data, out); err != nil {
			return model.Errf(http.StatusBadGateway, model.CodeUpstream, "unexpected dhan response: %v", err)
		}
	}
	return nil
}

// mapHTTPError converts a Dhan error response into an APIError, keeping the
// raw body in Detail so the client sees exactly what the broker said.
func mapHTTPError(status int, body []byte) *model.APIError {
	var d struct {
		ErrorType    string `json:"errorType"`
		ErrorCode    string `json:"errorCode"`
		ErrorMessage string `json:"errorMessage"`
	}
	_ = json.Unmarshal(body, &d)
	msg := d.ErrorMessage
	if msg == "" {
		msg = strings.TrimSpace(string(body))
		if len(msg) > 300 {
			msg = msg[:300]
		}
	}
	if msg == "" {
		msg = http.StatusText(status)
	}
	e := &model.APIError{Message: msg}
	if json.Valid(body) {
		e.Detail = append(json.RawMessage(nil), body...)
	}
	switch {
	case status == 401 || status == 403:
		e.HTTPStatus, e.Code = http.StatusFailedDependency, model.CodeBrokerAuth
	case status == 429:
		e.HTTPStatus, e.Code = http.StatusTooManyRequests, model.CodeRateLimited
	case status >= 400 && status < 500:
		e.HTTPStatus, e.Code = status, model.CodeBrokerRejected
	default:
		e.HTTPStatus, e.Code = http.StatusBadGateway, model.CodeUpstream
	}
	return e
}

// decodeOne decodes a JSON object or a single-element array into out.
func decodeOne[T any](raw json.RawMessage) (*T, error) {
	raw = bytes.TrimSpace(raw)
	var v T
	if len(raw) > 0 && raw[0] == '[' {
		var arr []T
		if err := json.Unmarshal(raw, &arr); err != nil {
			return nil, err
		}
		if len(arr) == 0 {
			return nil, model.NotFound("not found")
		}
		return &arr[0], nil
	}
	if err := json.Unmarshal(raw, &v); err != nil {
		return nil, err
	}
	return &v, nil
}
