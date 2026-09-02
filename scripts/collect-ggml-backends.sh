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
    | xargs -0 ls -td 2>/dev/null | head -1
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
found_libs=0
for f in "${out_dir}"/lib*/libggml-base.so.* "${out_dir}"/lib*/libggml.so.* \
         "${out_dir}"/lib*/libllama.so.* "${out_dir}"/lib*/libllama-common.so.* \
         "${out_dir}"/lib*/libggml-base.*.dylib "${out_dir}"/lib*/libggml.*.dylib \
         "${out_dir}"/lib*/libllama.*.dylib "${out_dir}"/lib*/libllama-common.*.dylib \
         "${out_dir}"/bin/ggml-base.dll "${out_dir}"/bin/ggml.dll \
         "${out_dir}"/bin/llama.dll "${out_dir}"/bin/llama-common.dll; do
  [ -f "$f" ] || continue
  case "$(basename "$f")" in
    # skip the fully-versioned unix spellings (libggml.so.0.18.0); keep .so.0
    *.so.*.*.*) continue ;;
    *.[0-9]*.[0-9]*.[0-9]*.dylib) continue ;;
  esac
  cp -f "$f" "${dest}/"
  found_libs=$((found_libs + 1))
done
if [ "${found_libs}" -eq 0 ]; then
  echo "error: no core ggml/llama shared libs found under ${out_dir}" >&2
  exit 1
fi

cp -f "${backends_dir}"/* "${dest}/"

echo "collected into ${dest}:"
ls -l "${dest}" | tail -n +2 | awk '{printf "  %s (%d bytes)\n", $NF, $5}'
