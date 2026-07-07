# Caddy with the Cloudflare DNS plugin — required for the DNS-01 challenge
# that issues the *.{DOMAIN} wildcard certificate (DEPLOY.md §2).
FROM caddy:2-builder AS builder
RUN xcaddy build --with github.com/caddy-dns/cloudflare

FROM caddy:2
COPY --from=builder /usr/bin/caddy /usr/bin/caddy
