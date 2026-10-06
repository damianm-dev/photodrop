FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY static static
RUN cargo build --release --locked

FROM debian:trixie-slim
COPY --from=build /src/target/release/photodrop /usr/local/bin/photodrop
WORKDIR /data
EXPOSE 8000
CMD ["photodrop"]
