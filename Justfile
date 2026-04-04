fmt:
    cargo +nightly fmt 

fix:
    cargo +nightly clippy --fix --allow-dirty

lint:
    cargo +nightly clippy -- -D warnings
