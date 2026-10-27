#!/usr/bin/env bash

# Download pinned build inputs with bounded retries and optional mirror fallback.
# Only replace the destination after its checksum has been verified.
download_musl_archive() {
  local expected_sha256="$1"
  local archive="$2"
  shift 2
  local partial_archive="${archive}.partial"
  local download_url

  for download_url in "$@"; do
    echo "Downloading ${download_url}"
    if curl --fail --silent --show-error --location \
      --connect-timeout 10 --max-time 120 \
      --retry 3 --retry-delay 2 --retry-max-time 180 --retry-all-errors \
      --output "${partial_archive}" "${download_url}" &&
      printf '%s  %s\n' "${expected_sha256}" "${partial_archive}" | sha256sum -c -; then
      mv -- "${partial_archive}" "${archive}" || return 1
      return 0
    fi
    echo "Download or checksum verification failed for ${download_url}" >&2
  done

  rm -f -- "${partial_archive}"
  echo "Could not download a verified archive from any configured URL" >&2
  return 1
}
