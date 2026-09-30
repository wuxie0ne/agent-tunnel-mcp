package config

import "testing"

func TestExplicitChoices(t *testing.T) {
	c := DefaultTarget()
	if c.Validate() == nil {
		t.Fatal("missing mode accepted")
	}
	c.Mode = "allow"
	if c.Validate() == nil {
		t.Fatal("missing transport accepted")
	}
	c.Transport = "quick"
	if e := c.Validate(); e != nil {
		t.Fatal(e)
	}
	c.Concurrency = 0
	if c.Validate() == nil {
		t.Fatal("unbounded concurrency")
	}
}
