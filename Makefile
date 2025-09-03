build:
	@cargo build

test:
	@cargo nextest run --all-features

release:
	@git cliff -o CHANGELOG.md
	@git commit -a -n -m "Update CHANGELOG.md" || true
	@git push origin master --tag

update-submodule:
	@git submodule update --init --recursive --remote

check:
	@cargo clippy --all-targets --all-features --tests --benches -- -D warnings

.PHONY: build test release update-submodule check
