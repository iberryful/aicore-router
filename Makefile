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

.PHONY: build test release update-submodule
