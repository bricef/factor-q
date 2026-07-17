package main

import (
	"encoding/json"
	"fmt"
	"strings"
	"time"
)

// RenderPayload applies the two supported substitutions and returns JSON bytes.
// A job without a payload returns nil, representing an empty (JSON null) body.
func RenderPayload(job Job, scheduled time.Time) ([]byte, error) {
	replace := func(s string) string {
		s = strings.ReplaceAll(s, "{{scheduled_time}}", scheduled.Format(time.RFC3339))
		return strings.ReplaceAll(s, "{{job}}", job.Name)
	}
	if job.PayloadJSON != nil {
		out := []byte(replace(*job.PayloadJSON))
		if !json.Valid(out) {
			return nil, fmt.Errorf("job %q: rendered payload_json is not valid JSON", job.Name)
		}
		return out, nil
	}
	if job.Payload == nil {
		return nil, nil
	}
	out, err := json.Marshal(substitute(job.Payload, replace))
	if err != nil {
		return nil, fmt.Errorf("job %q: encode payload: %w", job.Name, err)
	}
	return out, nil
}

func substitute(value any, replace func(string) string) any {
	switch value := value.(type) {
	case string:
		return replace(value)
	case map[string]any:
		out := make(map[string]any, len(value))
		for k, v := range value {
			out[k] = substitute(v, replace)
		}
		return out
	case []map[string]any:
		out := make([]map[string]any, len(value))
		for i, v := range value {
			out[i] = substitute(v, replace).(map[string]any)
		}
		return out
	case []any:
		out := make([]any, len(value))
		for i, v := range value {
			out[i] = substitute(v, replace)
		}
		return out
	default:
		return value
	}
}
