package main

import (
	"encoding/json"
	"fmt"
	"sort"
	"time"

	"github.com/bmatcuk/doublestar/v4"
	"gopkg.in/yaml.v3"
)

// Tier is how much human supervision a change to an area needs. The
// values are ordered by restrictiveness so the most restrictive tier of
// a PR's changed files is simply the maximum — see Verdict.
type Tier int

const (
	// TierUnsupervised: a merge here needs no human read of the diff.
	TierUnsupervised Tier = iota
	// TierSupervised: a human reads the change before it merges.
	TierSupervised
	// TierNever: a human merges it, always.
	TierNever
)

// tierNames is the wire vocabulary, indexed by Tier. It is the only
// spelling accepted in `.github/merge-policy.yml` and the only one
// written to a label or a comment.
var tierNames = [...]string{"unsupervised", "supervised", "never"}

func (t Tier) String() string {
	if int(t) < 0 || int(t) >= len(tierNames) {
		return fmt.Sprintf("tier(%d)", int(t))
	}
	return tierNames[t]
}

// MarshalJSON writes the wire vocabulary, not the ordinal: the `verdict`
// subcommand's output is read by a script and by a person, and a tier
// that crossed the seam as `2` would have to be decoded by a second copy
// of this list.
func (t Tier) MarshalJSON() ([]byte, error) { return json.Marshal(t.String()) }

// UnmarshalJSON reads the same vocabulary, refusing anything else.
func (t *Tier) UnmarshalJSON(data []byte) error {
	var name string
	if err := json.Unmarshal(data, &name); err != nil {
		return err
	}
	parsed, err := ParseTier(name)
	if err != nil {
		return err
	}
	*t = parsed
	return nil
}

// Label is the GitHub label that carries this tier's verdict.
func (t Tier) Label() string { return "merge:" + t.String() }

// TierLabels are the three verdict labels, in tier order. Exactly one is
// ever applied to a PR; the sweep removes the other two.
var TierLabels = []string{TierUnsupervised.Label(), TierSupervised.Label(), TierNever.Label()}

// ParseTier maps the wire vocabulary to a Tier. An unrecognised spelling
// is an error rather than a default: a policy file that says `unsuperivsed`
// must refuse to load, not quietly fall through to something permissive.
func ParseTier(s string) (Tier, error) {
	for i, name := range tierNames {
		if s == name {
			return Tier(i), nil
		}
	}
	return 0, fmt.Errorf("unknown tier %q (want one of %v)", s, tierNames)
}

// Areas is the repository's area declaration — `.github/areas.yml`, a map
// of area name to the globs that belong to it. It is the same file and
// the same format dorny/paths-filter reads in CI; see the file's header
// for why nothing may copy these globs inline.
type Areas map[string][]string

// ParseAreas decodes `.github/areas.yml`.
//
// The format is picomatch globs under dorny/paths-filter's `filters`
// syntax; matching here uses github.com/bmatcuk/doublestar, whose `**`
// semantics agree with picomatch's on everything this repo's globs use:
// `**` spans path separators, `*` does not, and both treat a leading dot
// as an ordinary character (picomatch only with `dot: true`, which is what
// paths-filter sets). The one known divergence is that doublestar's
// `a/**` also matches the bare directory `a` while picomatch's does not —
// inert here, because every path matched against these globs is a file
// path from a PR's changed-file list and never a bare directory.
//
// paths-filter also accepts a per-area object form (`{paths: [...]}`) and
// a rule with a change type. This repo uses neither, and a file that did
// would fail to decode here rather than be silently half-read.
func ParseAreas(data []byte) (Areas, error) {
	var areas Areas
	if err := yaml.Unmarshal(data, &areas); err != nil {
		return nil, fmt.Errorf("parse areas.yml: %w", err)
	}
	if len(areas) == 0 {
		return nil, fmt.Errorf("parse areas.yml: no areas declared")
	}
	for name, globs := range areas {
		if len(globs) == 0 {
			return nil, fmt.Errorf("parse areas.yml: area %q declares no globs", name)
		}
		for _, g := range globs {
			if !doublestar.ValidatePattern(g) {
				return nil, fmt.Errorf("parse areas.yml: area %q has an invalid glob %q", name, g)
			}
		}
	}
	return areas, nil
}

// Match returns the names of every area whose globs match path, sorted so
// a verdict's file table is byte-identical between two runs over the same
// facts (the comment is rewritten only when it changes).
func (a Areas) Match(path string) []string {
	var hit []string
	for name, globs := range a {
		for _, g := range globs {
			if ok, err := doublestar.Match(g, path); err == nil && ok {
				hit = append(hit, name)
				break
			}
		}
	}
	sort.Strings(hit)
	return hit
}

// Policy is `.github/merge-policy.yml`: the tier each area carries, the
// tier for a file that matches no mapped area, and the minimum PR age the
// structural checks report on.
type Policy struct {
	DefaultTier Tier
	MinAge      time.Duration
	Areas       map[string]Tier
	// InReviewLabel and HoldLabel are not in the file: they are the
	// watcher's own label vocabulary (Config), carried here so Verdict
	// stays pure over exactly three arguments and so the `verdict` seam
	// can ship a whole policy as one value. Empty means the repository
	// defaults — see inReviewLabel/holdLabel in verdict.go.
	InReviewLabel string
	HoldLabel     string
}

// policyFile is the on-disk shape, decoded before it is validated. The
// tiers arrive as strings so an unknown one can be named in the error.
type policyFile struct {
	DefaultTier   *string           `yaml:"default_tier"`
	MinAgeMinutes *int              `yaml:"min_age_minutes"`
	Areas         map[string]string `yaml:"areas"`
}

// ParsePolicy decodes and validates the merge policy against the areas it
// claims to be about.
//
// Every failure here is a refusal, never a partial load. The policy's job
// is to say which changes a human must read, so a version skew between it
// and areas.yml — an area renamed on one side, a tier misspelled — has to
// stop the sweep rather than relax it: a dropped mapping silently demotes
// everything that area covers to the default tier, and the only evidence
// would be a label that looks like a normal answer.
func ParsePolicy(data []byte, areas Areas) (Policy, error) {
	var raw policyFile
	if err := yaml.Unmarshal(data, &raw); err != nil {
		return Policy{}, fmt.Errorf("parse merge-policy.yml: %w", err)
	}
	if raw.DefaultTier == nil {
		return Policy{}, fmt.Errorf("parse merge-policy.yml: default_tier is required")
	}
	def, err := ParseTier(*raw.DefaultTier)
	if err != nil {
		return Policy{}, fmt.Errorf("parse merge-policy.yml: default_tier: %w", err)
	}
	if raw.MinAgeMinutes == nil {
		return Policy{}, fmt.Errorf("parse merge-policy.yml: min_age_minutes is required")
	}
	if *raw.MinAgeMinutes < 0 {
		return Policy{}, fmt.Errorf("parse merge-policy.yml: min_age_minutes must be >= 0, got %d", *raw.MinAgeMinutes)
	}
	if len(raw.Areas) == 0 {
		return Policy{}, fmt.Errorf("parse merge-policy.yml: areas maps nothing")
	}
	mapped := make(map[string]Tier, len(raw.Areas))
	for _, name := range sortedKeys(raw.Areas) {
		if _, ok := areas[name]; !ok {
			return Policy{}, fmt.Errorf("parse merge-policy.yml: area %q is not declared in areas.yml", name)
		}
		tier, err := ParseTier(raw.Areas[name])
		if err != nil {
			return Policy{}, fmt.Errorf("parse merge-policy.yml: area %q: %w", name, err)
		}
		mapped[name] = tier
	}
	return Policy{
		DefaultTier: def,
		MinAge:      time.Duration(*raw.MinAgeMinutes) * time.Minute,
		Areas:       mapped,
	}, nil
}

// sortedKeys gives map iteration a fixed order, so the first refusal a
// broken policy file reports is the same one every time.
func sortedKeys(m map[string]string) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}
