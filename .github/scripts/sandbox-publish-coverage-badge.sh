#!/usr/bin/env bash
# Publish a shields.io endpoint JSON file to the repository's `badges` branch.
#
# Usage (from repository root, with GITHUB_TOKEN and GITHUB_REPOSITORY set):
#   bash .github/scripts/sandbox-publish-coverage-badge.sh \
#     <lcov.info> <badge-file.json> <label>
#
# Computes line coverage from lcov LF/LH records, writes endpoint JSON, and
# force-updates only that file on `badges`. Does not modify other branches.
# Display only: not a coverage gate and not isolation evidence.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <lcov.info> <badge-file.json> <label>" >&2
  exit 2
fi

lcov_path=$1
badge_file=$2
label=$3

case "${badge_file}" in
  *.json) ;;
  *)
    echo "badge file must end in .json: ${badge_file}" >&2
    exit 2
    ;;
esac

if [[ ! -f "${lcov_path}" ]]; then
  echo "lcov file not found: ${lcov_path}" >&2
  exit 1
fi

if [[ -z "${GITHUB_TOKEN:-}" || -z "${GITHUB_REPOSITORY:-}" ]]; then
  echo "GITHUB_TOKEN and GITHUB_REPOSITORY are required" >&2
  exit 1
fi

read -r lines_hit lines_found < <(
  awk -F: '
    /^LF:/ { lf += $2 }
    /^LH:/ { lh += $2 }
    END { printf "%d %d\n", lh, lf }
  ' "${lcov_path}"
)

if [[ "${lines_found}" -le 0 ]]; then
  echo "lcov contained no line records" >&2
  exit 1
fi

# One decimal place, matching common llvm-cov summaries.
percent=$(
  awk -v lh="${lines_hit}" -v lf="${lines_found}" \
    'BEGIN { printf "%.1f", 100.0 * lh / lf }'
)
message="${percent}%"

# Brand green at/above 70%; amber then red below so low coverage is visible.
color=$(
  awk -v p="${percent}" 'BEGIN {
    if (p + 0 < 50) print "e05d44"
    else if (p + 0 < 70) print "dfb317"
    else print "218D76"
  }'
)

workdir=$(mktemp -d)
trap 'rm -rf "${workdir}"' EXIT

remote_url="https://github.com/${GITHUB_REPOSITORY}.git"
auth_header="AUTHORIZATION: bearer ${GITHUB_TOKEN}"
clone_dir="${workdir}/repo"

git_auth() {
  git -c "http.https://github.com/.extraheader=${auth_header}" "$@"
}

if git_auth ls-remote --exit-code --heads "${remote_url}" badges >/dev/null 2>&1; then
  git_auth clone --depth 1 --branch badges "${remote_url}" "${clone_dir}"
else
  git_auth clone --depth 1 "${remote_url}" "${clone_dir}"
  git -C "${clone_dir}" checkout --orphan badges
  # Drop the default-branch tree so badges only carries endpoint JSON.
  git -C "${clone_dir}" rm -rf --ignore-unmatch . >/dev/null 2>&1 || true
  find "${clone_dir}" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
fi

cd "${clone_dir}"
git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"

python3 - "${badge_file}" "${label}" "${message}" "${color}" <<'PY'
import json
import sys

path, label, message, color = sys.argv[1:5]
payload = {
    "schemaVersion": 1,
    "label": label,
    "message": message,
    "color": color,
}
with open(path, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY

git add -- "${badge_file}"
if git diff --cached --quiet; then
  echo "badge unchanged: ${badge_file} (${message})"
  exit 0
fi

git commit -m "chore(badges): updated ${badge_file} to ${message}"
git_auth push origin HEAD:refs/heads/badges
echo "published ${badge_file}: ${label} ${message}"
