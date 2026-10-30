FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM alpine:3
RUN apk add --no-cache ca-certificates \
    && adduser -S -D -H -h /data gpodder \
    && mkdir /data && chown gpodder /data
COPY --from=build /src/target/release/nc-gpodder-lite /usr/local/bin/nc-gpodder-lite
USER gpodder
WORKDIR /data
ENV NCGL_DB=/data/nc-gpodder-lite.db \
    NCGL_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["nc-gpodder-lite"]
CMD ["serve"]
