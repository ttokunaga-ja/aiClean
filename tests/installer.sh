#!/bin/sh
# Isolated installer fixtures: no network, PATH/profile edits, or user install.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
python3 - "$root" <<'PY'
import hashlib, os, pathlib, subprocess, sys, tempfile
root = pathlib.Path(sys.argv[1])
with tempfile.TemporaryDirectory(prefix='aiclean-installer-test-') as tmp:
    base = pathlib.Path(tmp)
    mocks = base / 'mocks'; mocks.mkdir()
    (mocks / 'uname').write_text('#!/bin/sh\necho Darwin\n')
    (mocks / 'curl').write_text('''#!/bin/sh
set -eu
out=
while [ "$#" -gt 0 ]; do
 case "$1" in
  -o|--output) out=$2; shift 2 ;;
  -w|--write-out) shift 2 ;;
  *) url=$1; shift ;;
 esac
done
case "$url" in
 */releases/latest) cat "$FIXTURE/release-url" ;;
 */SHA256SUMS) cp "$FIXTURE/SHA256SUMS" "$out" ;;
 */aiClean-macos) cp "$FIXTURE/aiClean-macos" "$out" ;;
 *) exit 99 ;;
esac
''')
    for path in mocks.iterdir(): path.chmod(0o755)
    fixture = base / 'fixture'; fixture.mkdir()
    (fixture / 'release-url').write_text('https://github.com/ttokunaga-ja/aiClean/releases/tag/v0.2.0')
    bin_dir = base / "bin space'$"; bin_dir.mkdir()
    installed = bin_dir / 'aiClean'
    env = dict(os.environ, PATH=str(mocks) + ':' + os.environ['PATH'],
               BIN_DIR=str(bin_dir), FIXTURE=str(fixture), AICLEAN_INSTALL_NO_PATH='1')
    def attempt(body, manifest=None, success=False):
        installed.write_bytes(b'original')
        asset = fixture / 'aiClean-macos'; asset.write_text('#!/bin/sh\n' + body + '\n')
        digest = hashlib.sha256(asset.read_bytes()).hexdigest()
        sums = manifest(digest) if manifest else f'{digest}  aiClean-macos\n'
        (fixture / 'SHA256SUMS').write_text(sums)
        result = subprocess.run(['sh', str(root / 'install.sh')], env=env,
                                text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=15)
        assert (result.returncode == 0) == success, (result.stdout, result.stderr)
        assert installed.read_bytes() == (asset.read_bytes() if success else b'original')
        assert sorted(p.name for p in bin_dir.iterdir()) == ['aiClean']
    attempt("printf 'aiClean 0.2.0\\n'", success=True)
    attempt("printf 'aiClean 0.2.1\\n'")
    attempt("printf 'aiClean 0.2.0\\n'; exit 1")
    attempt("printf 'aiClean 0.2.0\\n'", lambda h: f'{h}  aiClean-macos\n{h}  aiClean-macos\n')
    attempt("printf 'aiClean 0.2.0\\n'", lambda h: f'{h}  aiClean-macos\nbroken\n')
    attempt("printf 'aiClean 0.2.0\\n'", lambda h: '0' * 64 + '  aiClean-macos\n')
    attempt("printf 'aiClean 0.2.0\\n'", lambda h: f'{h}  ../aiClean-macos\n')
    for tag in ['v01.2.0', 'v0.2.0-rc.1', 'v0.2.0/extra']:
        (fixture / 'release-url').write_text('https://github.com/ttokunaga-ja/aiClean/releases/tag/' + tag)
        attempt("printf 'aiClean 0.2.0\\n'")
    (fixture / 'release-url').write_text('https://github.com/ttokunaga-ja/aiClean/releases/tag/v0.2.0')
    attempt("exec sleep 30")
    attempt("printf 'aiClean 0.2.0\\n'", success=True)
    home = base / 'isolated-home'; home.mkdir()
    profile_env = dict(env, HOME=str(home), SHELL='/bin/zsh', ZDOTDIR=str(home), AICLEAN_INSTALL_NO_PATH='0')
    for _ in range(2):
        result = subprocess.run(['sh', str(root / 'install.sh')], env=profile_env, capture_output=True, timeout=15)
        assert result.returncode == 0, result.stderr
    profile = home / '.zshrc'
    assert profile.read_text().count('# aiClean installer') == 1
    result = subprocess.run(['sh', '-c', '. "$1"; printf "%s" "$PATH"', 'fixture', str(profile)],
                            env=dict(env, PATH='/usr/bin:/bin'), capture_output=True, text=True)
    assert result.returncode == 0 and result.stdout.split(':')[0] == str(bin_dir)
    installed.unlink(); installed.symlink_to(fixture / 'aiClean-macos')
    result = subprocess.run(['sh', str(root / 'install.sh')], env=env, capture_output=True, timeout=15)
    assert result.returncode != 0 and installed.is_symlink()
print('installer fixtures passed (valid staging, malformed/duplicate/hash/version/tag/timeout failures, isolated PATH quoting/idempotence, symlink refusal)')
PY
