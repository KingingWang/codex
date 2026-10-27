"""Offline regression tests for checksum-verified musl input downloads."""

import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("download-musl-archive.sh")
PAYLOAD = b"trusted archive payload"
SHA256 = hashlib.sha256(PAYLOAD).hexdigest()
SUCCESS = "https://success.invalid/archive"
NETWORK_ERROR = "https://network-error.invalid/archive"
BAD_CHECKSUM = "https://bad-checksum.invalid/archive"

HARNESS = r"""
set -euo pipefail
source "$1"
shift
curl() {
  printf '%s\n' "$@" >> "${MOCK_CURL_CALLS}"
  local output="" url=""
  while (( $# )); do
    case "$1" in
      --output) output="$2"; shift 2 ;;
      https://*) url="$1"; shift ;;
      *) shift ;;
    esac
  done
  case "${url}" in
    https://network-error.invalid/*)
      printf 'partial download' > "${output}"
      return 28
      ;;
    https://bad-checksum.invalid/*)
      printf 'corrupt archive' > "${output}"
      ;;
    https://success.invalid/*)
      printf '%s' "${MOCK_PAYLOAD}" > "${output}"
      ;;
    *) return 1 ;;
  esac
}
download_musl_archive "$@"
"""


class DownloadMuslArchiveTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.archive = self.root / "archive with spaces.tar.xz"
        self.calls = self.root / "curl-calls"

    def download(self, *urls):
        result = subprocess.run(
            [
                "bash",
                "-c",
                HARNESS,
                "test",
                str(SCRIPT),
                SHA256,
                str(self.archive),
                *urls,
            ],
            env={
                **os.environ,
                "MOCK_CURL_CALLS": str(self.calls),
                "MOCK_PAYLOAD": PAYLOAD.decode(),
            },
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.arguments = self.calls.read_text().splitlines()
        self.requested_urls = [arg for arg in self.arguments if arg.startswith("https://")]
        return result

    def assert_no_partial_archive(self):
        self.assertFalse(Path(f"{self.archive}.partial").exists())

    def test_success_verifies_checksum_and_skips_remaining_mirrors(self):
        result = self.download(SUCCESS, NETWORK_ERROR)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.archive.read_bytes(), PAYLOAD)
        self.assertEqual(self.requested_urls, [SUCCESS])
        self.assert_no_partial_archive()

    def test_curl_uses_bounded_timeouts_and_retries_connection_errors(self):
        self.download(SUCCESS)
        for flag, value in [
            ("--connect-timeout", "10"),
            ("--max-time", "120"),
            ("--retry", "3"),
            ("--retry-max-time", "180"),
        ]:
            self.assertEqual(self.arguments[self.arguments.index(flag) + 1], value)
        self.assertIn("--retry-all-errors", self.arguments)
        self.assertIn("--fail", self.arguments)

    def test_connection_failure_falls_back_to_the_next_mirror(self):
        result = self.download(NETWORK_ERROR, SUCCESS)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.archive.read_bytes(), PAYLOAD)
        self.assertEqual(self.requested_urls, [NETWORK_ERROR, SUCCESS])
        self.assert_no_partial_archive()

    def test_checksum_failure_falls_back_to_the_next_mirror(self):
        result = self.download(BAD_CHECKSUM, SUCCESS)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.archive.read_bytes(), PAYLOAD)
        self.assertEqual(self.requested_urls, [BAD_CHECKSUM, SUCCESS])
        self.assert_no_partial_archive()

    def test_all_connection_failures_are_reported_and_cleaned_up(self):
        result = self.download(NETWORK_ERROR, f"{NETWORK_ERROR}/fallback")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.archive.exists())
        self.assert_no_partial_archive()

    def test_all_checksum_failures_are_rejected(self):
        result = self.download(BAD_CHECKSUM, f"{BAD_CHECKSUM}/fallback")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.archive.exists())
        self.assert_no_partial_archive()

    def test_failed_download_does_not_overwrite_an_existing_archive(self):
        self.archive.write_bytes(b"existing verified archive")
        result = self.download(NETWORK_ERROR, BAD_CHECKSUM)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.archive.read_bytes(), b"existing verified archive")
        self.assert_no_partial_archive()


if __name__ == "__main__":
    unittest.main()
