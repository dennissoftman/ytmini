#!/usr/bin/env python3
"""Copy libmpv + the mpv binary and every non-system dylib they need into a
.app bundle, and rewrite load paths so nothing points at Homebrew anymore.

    scripts/bundle_dylibs.py dist/ytmini.app [--prefix /opt/homebrew]

Result:
  Contents/Frameworks/libmpv.2.dylib  (+ ~50 deps: ffmpeg, libass, codecs…)
  Contents/Helpers/mpv                (separate-window fallback player)

Every reference becomes @rpath/<name>; dylibs search @loader_path, the mpv
binary @executable_path/../Frameworks. Old rpaths are dropped except the
system Swift runtime (/usr/lib/swift). Each file is ad-hoc re-signed because
install_name_tool invalidates signatures (arm64 refuses to load unsigned code).
"""
import os
import shutil
import subprocess
import sys

SYSTEM_PREFIXES = ("/usr/lib/", "/System/")
KEEP_RPATHS = {"/usr/lib/swift"}


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def deps(path):
    lines = run("otool", "-L", path).splitlines()[1:]
    return [l.strip().split(" (")[0] for l in lines]


def rpaths(path):
    out = run("otool", "-l", path).split("cmd LC_RPATH")[1:]
    return [chunk.split("path ")[1].split(" (")[0] for chunk in out]


def main():
    app = sys.argv[1]
    prefix = sys.argv[sys.argv.index("--prefix") + 1] if "--prefix" in sys.argv else "/opt/homebrew"
    fw = os.path.join(app, "Contents", "Frameworks")
    helpers = os.path.join(app, "Contents", "Helpers")
    os.makedirs(fw, exist_ok=True)
    os.makedirs(helpers, exist_ok=True)

    def is_bundled_dep(d):
        return d.startswith(prefix) or d.startswith("@rpath/")

    def resolve(ref):
        """A load path → the real file on disk."""
        if ref.startswith("@rpath/"):
            cand = os.path.join(prefix, "lib", ref[len("@rpath/"):])
            if not os.path.exists(cand):
                sys.exit(f"cannot resolve {ref} (looked in {prefix}/lib)")
            return os.path.realpath(cand)
        return os.path.realpath(ref)

    libmpv = os.path.join(prefix, "lib", "libmpv.2.dylib")
    mpv_bin = os.path.realpath(os.path.join(prefix, "bin", "mpv"))
    if not os.path.exists(libmpv):
        sys.exit(f"{libmpv} not found — brew install mpv")

    # name in bundle -> source file
    names = {"libmpv.2.dylib": os.path.realpath(libmpv)}
    queue = [os.path.realpath(libmpv), mpv_bin]
    seen = set()
    while queue:
        src = queue.pop()
        if src in seen:
            continue
        seen.add(src)
        for d in deps(src):
            if is_bundled_dep(d):
                name = os.path.basename(d)
                real = resolve(d)
                if names.setdefault(name, real) != real and os.path.basename(names[name]) != os.path.basename(real):
                    print(f"warning: {name} maps to two files; using {names[name]}")
                queue.append(real)

    # copy
    targets = []
    for name, src in sorted(names.items()):
        dst = os.path.join(fw, name)
        shutil.copyfile(src, dst)
        os.chmod(dst, 0o755)
        targets.append((dst, "@loader_path"))
    mpv_dst = os.path.join(helpers, "mpv")
    shutil.copyfile(mpv_bin, mpv_dst)
    os.chmod(mpv_dst, 0o755)
    targets.append((mpv_dst, "@executable_path/../Frameworks"))

    # relink
    for path, own_rpath in targets:
        ops = []  # tuples of install_name_tool arguments, deduplicated below
        if path.endswith(".dylib"):
            ops.append(("-id", "@rpath/" + os.path.basename(path)))
        own_id = path.endswith(".dylib") and deps(path)[0]  # otool -L lists a dylib's own id first
        for d in deps(path):
            if is_bundled_dep(d) and d != own_id:
                ops.append(("-change", d, "@rpath/" + os.path.basename(d)))
        for rp in rpaths(path):
            if rp not in KEEP_RPATHS:
                ops.append(("-delete_rpath", rp))
        if own_rpath not in rpaths(path):
            ops.append(("-add_rpath", own_rpath))
        clean = [arg for op in dict.fromkeys(ops) for arg in op]
        subprocess.run(["install_name_tool", *clean, path], check=True, capture_output=True)
        subprocess.run(["codesign", "--force", "--sign", "-", path], check=True, capture_output=True)

    # verify: nothing may still point into the prefix
    bad = []
    for path, _ in targets:
        for d in deps(path):
            if d.startswith(prefix):
                bad.append(f"{os.path.basename(path)} -> {d}")
        for rp in rpaths(path):
            if rp.startswith(prefix) or "Xcode.app" in rp:
                bad.append(f"{os.path.basename(path)} rpath {rp}")
    if bad:
        sys.exit("still linked to the build machine:\n  " + "\n  ".join(bad))

    size = sum(os.path.getsize(p) for p, _ in targets)
    print(f"bundled {len(targets) - 1} dylibs + mpv ({size / 1048576:.0f} MB)")


if __name__ == "__main__":
    main()
