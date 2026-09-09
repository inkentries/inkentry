#!/usr/bin/env bash
# Collect the llama.cpp runtime files a `llama-vulkan` build produces into one
# directory, so release packaging can ship them next to the binaries.
#
# A dynamic-backends build splits the engine three ways:
#   - the rust binary (links the core libs below by SONAME, via $ORIGIN rpath)
#   - core shared libs: libggml-base, libggml, libllama, libllama-common
#   - backend modules (libggml-vulkan, libggml-cpu-<variant>, ...), dlopen'd at
#     runtime; a module whose driver is absent fails to load and is skipped
#
# The libs and modules land in llama-cpp-sys-2's cargo OUT_DIR, which carries a
# build hash. More than one such dir can exist (feature unification across
# builds); the one holding a `backends/` dir belongs to the dynamic-backends
# build, and the newest wins.
#
# usage: collect-ggml-backends.sh <cargo-target-profile-dir> <dest-dir>
#   e.g. collect-ggml-backends.sh target/x86_64-unknown-linux-gnu/release dist

set -euo pipefail

profile_dir="${1:?usage: collect-ggml-backends.sh <cargo-target-profile-dir> <dest-dir>}"
dest="${2:?usage: collect-ggml-backends.sh <cargo-target-profile-dir> <dest-dir>}"

backends_dir="$(
  find "${profile_dir}/build" -maxdepth 3 -type d \
    -path '*/llama-cpp-sys-2-*/out/backends' -print0 2>/dev/null \
    | xargs -0 -r ls -td 2>/dev/null | head -1
)"
if [ -z "${backends_dir}" ]; then
  echo "error: no llama-cpp-sys-2 backends dir under ${profile_dir}/build" \
       "(was the build run with --features llama-vulkan?)" >&2
  exit 1
fi
out_dir="$(dirname "${backends_dir}")"

mkdir -p "${dest}"

# Core libs by SONAME only (libfoo.so.0 / libfoo.0.dylib / foo.dll): the
# loader resolves by SONAME, so the un-versioned and fully-versioned spellings
# are dev-time symlinks the archive doesn't need.
#
# Assert the FULL required set, not just "found at least one". The .deb job
# hard-codes all four core libs at SOVERSION 0, so a llama.cpp bump that renames
# one or moves it off SOVERSION 0 would otherwise pass collection, the tarball
# and the glibc gate, and fail only in the deb job at tag-push. Requiring each
# lib here makes that break on the build leg instead.
missing=()
for lib in libggml-base libggml libllama libllama-common; do
  # The Windows DLL drops the `lib` prefix (ggml-base.dll, llama.dll, ...).
  win="${lib#lib}"
  count=0
  for f in "${out_dir}"/lib*/"${lib}".so.* \
           "${out_dir}"/lib*/"${lib}".*.dylib \
           "${out_dir}"/bin/"${win}".dll; do
    [ -f "$f" ] || continue
    case "$(basename "$f")" in
      # skip the fully-versioned unix spellings (libggml.so.0.18.0); keep .so.0
      *.so.*.*.*) continue ;;
      *.[0-9]*.[0-9]*.[0-9]*.dylib) continue ;;
    esac
    dst="${dest}/$(basename "$f")"
    # On Windows cargo hardlinks the core DLLs into the profile dir, which is also
    # our dest; plain cp then aborts with "are the same file". Drop the existing
    # dest entry first (a second hardlink to the same data — the source, a
    # different path, survives), so the copy always proceeds. A no-op elsewhere,
    # where the core libs are not already in the profile dir.
    rm -f "$dst"
    cp -f "$f" "$dst"
    count=$((count + 1))
  done
  [ "${count}" -gt 0 ] || missing+=("${lib}")
done
if [ "${#missing[@]}" -ne 0 ]; then
  echo "error: missing required core ggml/llama shared lib(s): ${missing[*]}" \
       "under ${out_dir} — a llama.cpp bump may have renamed them or moved them" \
       "off SOVERSION 0" >&2
  exit 1
fi

for f in "${backends_dir}"/*; do
  [ -e "$f" ] || continue
  dst="${dest}/$(basename "$f")"
  rm -f "$dst"
  cp -f "$f" "$dst"
done

echo "collected into ${dest}:"
ls -l "${dest}" | tail -n +2 | awk '{printf "  %s (%d bytes)\n", $NF, $5}'
