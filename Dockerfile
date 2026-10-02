# Three stages: compile the browser code, build the server, ship an image with
# TeX Live, poppler (pdftoppm/pdftotext for the AI) and git.

FROM node:22-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ .
RUN npx tsc

FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY web/editor.css ./web/editor.css
COPY --from=web /web/dist ./web/dist
RUN cargo build --release

FROM debian:bookworm-slim
# texlive-full is large (~5 GB) but means any package a paper needs is there;
# swap for texlive-latex-extra + texlive-fonts-recommended + texlive-bibtex-extra + biber to slim down.
RUN apt-get update && apt-get install -y --no-install-recommends \
      texlive-full latexmk git poppler-utils ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -u 1000 -m kloverleaf
WORKDIR /app
COPY --from=build /build/target/release/kloverleaf /usr/local/bin/kloverleaf
RUN mkdir projects && chown -R kloverleaf:kloverleaf /app
USER kloverleaf
ENV KLOVERLEAF_ROOT=/app KLOVERLEAF_BIND=0.0.0.0:7200
EXPOSE 7200
CMD ["kloverleaf"]
