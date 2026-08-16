SHELL := /usr/bin/env bash
.DEFAULT_GOAL := compare

SHRIMP := ../../target/debug/shrimp
WORK := .work

.PHONY: all bash nu shrimp cargo-build compare repeatability clean

all: compare

cargo-build:
	cargo build --manifest-path ../../Cargo.toml

bash:
	bash ./build.bash "$(WORK)/bash"

nu:
	nu ./build.nu "$(WORK)/nu"

shrimp: cargo-build
	$(SHRIMP) ./build.shrimp OUT="$(WORK)/shrimp"

compare: clean bash nu shrimp
	diff -ru "$(WORK)/bash" "$(WORK)/nu"
	diff -ru "$(WORK)/bash" "$(WORK)/shrimp"
	@printf 'all three workspaces match\n'

repeatability: clean bash nu shrimp
	cp -R "$(WORK)" "$(WORK).first"
	$(MAKE) bash nu shrimp
	diff -ru "$(WORK).first" "$(WORK)"
	@printf 'all implementations are repeatable\n'

clean:
	rm -rf "$(WORK)" "$(WORK).first"
