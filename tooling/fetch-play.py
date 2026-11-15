#!/usr/bin/env python3
"""Import our verified Bevy web build for Sites; never fall back to an npm engine."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import zipfile

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('play_artifact', ROOT / 'tooling/play-artifact.py')
artifact = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(artifact)
def repository():
    """The GitHub repository whose Bevy CI builds Play: this checkout's, named by Actions or
    by its origin remote. A clone of the public repository builds its own web app; it cannot
    read another repository's runs."""
    name = os.environ.get('GITHUB_REPOSITORY')
    if name:
        return name
    url = subprocess.run(['git', 'remote', 'get-url', 'origin'], cwd=ROOT,
                         capture_output=True, text=True).stdout.strip()
    found = re.search(r'github\.com[:/]([^/\s]+/[^/\s]+?)(?:\.git)?/?$', url)
    return found.group(1) if found else 'decentraland/dcl-one'


REPO = repository()
WORKFLOW = '.github/workflows/bevy-explorer-ci.yml'
PRODUCER = 'Web engine, HUD and bridge scene'
UPLOAD_STEP = 'Upload the complete web app'
MAX_BYTES = 2 * 1024**3


def server_error(stderr):
    # gh reports "Service Unavailable (HTTP 503)" for a JSON error and "HTTP 503" for a bare one.
    return re.search(r'\bHTTP (?:500|502|503|504)\b', stderr)


def rate_limited(stderr):
    # gh reports "API rate limit exceeded ... (HTTP 403)" and "... secondary rate limit ... (HTTP 403)".
    # A 403 without those words is a permission error and stays final.
    return re.search(r'\bHTTP 429\b', stderr) or (
        re.search(r'\bHTTP 403\b', stderr) and re.search(r'rate limit', stderr, re.IGNORECASE))


def read(request, deadline=None, limit=60, attempts=None, transient=server_error):
    # Retry reads within the caller's existing wait budget. Non-waiting imports
    # and final publication checks get only four retries; `attempts` caps the
    # tries in either mode. No writes are repeated. A rate limit backs off from
    # one minute, a server error from two seconds; both double up to a cap.
    attempt = 0
    while True:
        timeout = limit if deadline is None else min(limit, deadline - time.monotonic())
        if timeout <= 0:
            raise TimeoutError('GitHub read exceeded the configured wait deadline')
        try:
            return request(timeout)
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as exc:
            stderr = exc.stderr.decode(errors='replace') if isinstance(exc.stderr, bytes) else (exc.stderr or '')
            if isinstance(exc, subprocess.CalledProcessError) and stderr:
                print(stderr, file=sys.stderr, end='')
            exhausted = (attempts is not None and attempt + 1 >= attempts) or (
                attempt >= 4 if deadline is None else time.monotonic() >= deadline)
            limited = isinstance(exc, subprocess.CalledProcessError) and rate_limited(stderr)
            if exhausted or not (isinstance(exc, subprocess.TimeoutExpired) or limited or transient(stderr)):
                raise
            delay = min(240, 60 * 2 ** min(attempt, 2)) if limited else min(30, 2 ** min(attempt + 1, 5))
            if deadline is not None:
                delay = min(delay, max(0, deadline - time.monotonic()))
            cause = 'GitHub rate limit' if limited else 'Transient GitHub read failure'
            print(f'{cause}; retry {attempt + 1} in {delay:g}s.', flush=True)
            time.sleep(delay)
            if deadline is not None and time.monotonic() >= deadline:
                raise
            attempt += 1


def api(path, deadline=None):
    def request(timeout):
        return json.loads(subprocess.check_output(
            ['gh', 'api', '--method', 'GET', f'repos/{REPO}/{path}'],
            text=True, stderr=subprocess.PIPE, timeout=timeout))
    return read(request, deadline)


def download(record, archive):
    """Save an artifact archive; one failure short of a 4xx starts the file over.

    The wait bounds the search for a run, not its download: each of the two
    attempts gets 300 s, however little of the wait is left.
    """
    def request(timeout):
        with archive.open('wb') as output:
            subprocess.run(['gh', 'api', f'repos/{REPO}/actions/artifacts/{record["id"]}/zip'],
                           stdout=output, stderr=subprocess.PIPE, check=True, timeout=timeout)
    read(request, limit=300, attempts=2, transient=lambda stderr: not re.search(r'\bHTTP 4\d\d\b', stderr))


def workflow_runs(deadline=None):
    # Paginate without search filters: branch/head_sha searches cap results at
    # 1,000. Trust and PR identity are checked locally, on every returned run.
    page = 1
    while True:
        rows = api(f'actions/workflows/bevy-explorer-ci.yml/runs?per_page=100&page={page}', deadline)['workflow_runs']
        yield from rows
        if len(rows) < 100:
            return
        page += 1


def poll_runs(pending, deadline=None):
    # Discover recent producers without rescanning history. A pending run may
    # have fallen off page one, so refresh those IDs explicitly as well.
    recent = api('actions/workflows/bevy-explorer-ci.yml/runs?per_page=100&page=1', deadline)['workflow_runs']
    yield from recent
    listed = {run['id'] for run in recent}
    for run_id in sorted(pending - listed):
        yield api(f'actions/runs/{run_id}', deadline)


def current_pr():
    if os.environ.get('GITHUB_EVENT_NAME') != 'pull_request':
        return None
    event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
    merge, head = os.environ['GITHUB_SHA'], event['pull_request']['head']['sha']
    # The base bevy-ci.py selected the lanes with, so engine_changed() agrees with them.
    spec = importlib.util.spec_from_file_location('bevy_ci', Path(__file__).with_name('bevy-ci.py'))
    ci = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(ci)
    return {'number': event['number'], 'head': head,
            'base': ci.tested_base(merge, head, event['pull_request']['base']['sha']), 'merge': merge}


def inputs_at(revision):
    """The engine-input fingerprint of a commit, fetched shallowly if the checkout lacks it; None when unreadable."""
    try:
        if subprocess.run(['git', 'cat-file', '-e', f'{revision}^{{commit}}'],
                          cwd=ROOT, capture_output=True).returncode:
            subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', revision],
                           cwd=ROOT, capture_output=True, check=True, timeout=60)
        return artifact.inputs(ROOT, revision)
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def engine_changed(pr):
    """Whether the PR changes an engine input (engine source, protos, HUD, build scripts)
    relative to the commit it is based on. A PR that does not has main's engine, so it is
    tested with the newest main build that uploaded its web app and has not failed instead
    of waiting for main to rebuild the same inputs; a PR that does gets its own Bevy run and
    needs that exact build."""
    if not pr or not pr.get('base'):
        return True
    base = inputs_at(pr['base'])
    return base is None or base != artifact.inputs(ROOT)


def trusted(run, pr=None):
    if (run.get('path') != WORKFLOW or run.get('head_repository', {}).get('full_name') != REPO
            or not re.fullmatch('[0-9a-f]{40}', run.get('head_sha', ''))):
        return False
    if run['event'] in ('push', 'workflow_dispatch') and run['head_branch'] == 'main':
        return True
    return bool(pr and run['event'] == 'pull_request' and run['head_sha'] == pr['head']
                and any(row['number'] == pr['number'] for row in run.get('pull_requests', [])))


def extract(archive, output):
    with zipfile.ZipFile(archive) as source:
        members = source.infolist()
        if len(members) > 20000 or sum(m.file_size for m in members) > MAX_BYTES:
            raise ValueError('Explorer archive exceeds extraction limits')
        seen = set()
        for member in members:
            path = PurePosixPath(member.filename)
            mode = member.external_attr >> 16
            if (path.is_absolute() or '..' in path.parts or '.git' in path.parts or '\\' in member.filename
                    or not path.parts or path.as_posix() in seen
                    or stat.S_ISLNK(mode) or stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR)):
                raise ValueError(f'Unsafe Explorer archive entry: {member.filename}')
            seen.add(path.as_posix())
        source.extractall(output)


def install(source, production=False, producer=None):
    # producer: the commit whose inputs the build must match instead of the tree's (a
    # PR that changes no engine input importing main's newest build).
    meta = artifact.verify(source, ROOT, not production, producer)
    cache = ROOT / '.cache/play'
    cache.mkdir(parents=True, exist_ok=True)
    # Different optimization modes and producer commits never overwrite each other.
    key = artifact.digest(json.dumps(meta, sort_keys=True).encode())
    dest = cache / key
    with (cache / '.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not dest.exists():
            with tempfile.TemporaryDirectory(prefix='.import-', dir=cache) as temporary:
                stage = Path(temporary) / 'build'
                stage.mkdir()
                for part in (*artifact.PARTS, 'bindings', artifact.MANIFEST):
                    src = source / part
                    if src.is_dir():
                        shutil.copytree(src, stage / part)
                    else:
                        shutil.copy2(src, stage / part)
                artifact.verify(stage, ROOT, not production, producer)
                stage.rename(dest)
        if artifact.verify(dest, ROOT, not production, producer) != meta:
            raise ValueError('Cached Explorer manifest differs from the validated import')
        if producer:
            artifact.record_producer_validation(dest, producer, ROOT)
        link = cache / '.current-next'
        link.unlink(missing_ok=True)
        link.symlink_to(dest.name, target_is_directory=True)
        link.replace(cache / 'current')
    print(f"Sites uses our Explorer build {meta['build_revision']} at {dest}", flush=True)
    return dest


def producer_ready(job):
    # Upload runs only after every earlier step passed; later steps rebuild deploy/web for npm, not the artifact.
    if job['status'] == 'completed':
        return job['conclusion'] == 'success'
    uploads = [step for step in job.get('steps') or [] if step.get('name') == UPLOAD_STEP]
    return (job['status'] == 'in_progress' and len(uploads) == 1
            and uploads[0].get('status') == 'completed' and uploads[0].get('conclusion') == 'success')


class ProducerEnded(Exception):
    """The producer job of a run under way ended without an upload; the run can go on for other jobs."""


def producer_record(run, pr=None, deadline=None):
    jobs = api(f'actions/runs/{run["id"]}/jobs?per_page=100', deadline)
    selected = [job for job in jobs['jobs'] if job['name'] == PRODUCER]
    if jobs['total_count'] != len(jobs['jobs']) or len(selected) != 1:
        return None
    if not producer_ready(selected[0]):
        if run['status'] != 'completed' and selected[0]['status'] == 'completed':
            raise ProducerEnded(selected[0]['conclusion'])
        return None
    revision = pr['merge'] if run['event'] == 'pull_request' else run['head_sha']
    data = api(f'actions/runs/{run["id"]}/artifacts?per_page=100', deadline)
    name = f'bevy-explorer-web-{revision}'
    found = [row for row in data['artifacts'] if row['name'] == name and not row['expired']]
    if data['total_count'] != len(data['artifacts']) or len(found) != 1:
        return None
    record = found[0]
    checksum = record.get('digest') or ''
    if not checksum and run['status'] != 'completed':
        return None
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', checksum):
        raise ValueError('GitHub did not provide a verifiable artifact digest')
    if record['size_in_bytes'] > MAX_BYTES:
        raise ValueError('Explorer download exceeds size limit')
    return record


def download_run(run, pr=None, production=False, receipt=None, deadline=None, *, producer=None):
    # Preparation can overlap native tests. Only --production or a verified
    # receipt at publication authorizes delivery after the COMPLETE run passes.
    if production and (not trusted(run) or (run['status'] == 'completed' and run['conclusion'] != 'success') or (receipt is None and
            (run['status'] != 'completed' or run['conclusion'] != 'success'))):
        return None
    record = producer_record(run, pr, deadline)
    if record is None:
        return None
    revision = pr['merge'] if run['event'] == 'pull_request' else run['head_sha']
    checksum = record['digest']
    with tempfile.TemporaryDirectory(prefix='play-ci-') as temporary:
        tmp = Path(temporary)
        archive = tmp / 'web.zip'
        download(record, archive)
        with archive.open('rb') as stream:
            if hashlib.file_digest(stream, 'sha256').hexdigest() != checksum.split(':')[1]:
                raise ValueError('GitHub Explorer archive digest mismatch')
        extract(archive, tmp / 'web')
        meta = artifact.verify(tmp / 'web', ROOT, not production, producer)
        if meta['build_revision'] != revision:
            raise ValueError('Explorer metadata does not identify the validated producer revision')
        destination = install(tmp / 'web', production, producer)
        if receipt is not None:
            if not production:
                raise ValueError('Only optimized trusted-main imports can issue a production receipt')
            receipt.parent.mkdir(parents=True, exist_ok=True)
            receipt.write_text(json.dumps({'version': 1, 'run_id': run['id'], 'run_attempt': run['run_attempt'],
                'revision': revision, 'artifact_id': record['id'], 'digest': checksum,
                'inputs': artifact.inputs(ROOT)}, indent=2) + '\n')
        return destination


def verify_production(receipt, wait=0):
    """Recheck the exact producer used by a trusted image build, without downloading it again."""
    proof = json.loads(receipt.read_text())
    if (proof.get('version') != 1 or proof.get('inputs') != artifact.inputs(ROOT)
            or not re.fullmatch('[0-9a-f]{40}', proof.get('revision', ''))
            or not re.fullmatch('sha256:[0-9a-f]{64}', proof.get('digest', ''))
            or any(type(proof.get(k)) is not int or proof[k] <= 0 for k in ('run_id', 'run_attempt', 'artifact_id'))):
        raise ValueError('Invalid production receipt or changed Explorer inputs')
    deadline = time.monotonic() + wait
    read_deadline = deadline if wait > 0 else None
    poll_delay = 2
    while True:
        run = api(f'actions/runs/{proof["run_id"]}', read_deadline)
        if (not trusted(run) or run['id'] != proof['run_id'] or run['head_sha'] != proof['revision']
                or run['run_attempt'] != proof['run_attempt']):
            raise ValueError('Production producer identity or attempt changed; rebuild the images')
        if run['status'] == 'completed':
            if run['conclusion'] != 'success':
                raise ValueError('Complete Bevy CI did not pass; refusing publication')
            record = producer_record(run, deadline=read_deadline)
            if not record or (record['id'], record['digest']) != (proof['artifact_id'], proof['digest']):
                raise ValueError('Production artifact is missing, expired or changed; rebuild the images')
            print(f'Production producer {run["id"]}, attempt {run["run_attempt"]}, artifact {record["id"]}: complete CI passed.')
            return
        if time.monotonic() >= deadline:
            raise RuntimeError('Complete Bevy CI is still pending; refusing publication')
        print(f'Waiting for complete Bevy CI run {run["id"]} before publication.', flush=True)
        time.sleep(min(poll_delay, max(0, deadline - time.monotonic())))
        poll_delay = min(poll_delay * 2, 30)


def fetch(wait=0, production=False, receipt=None):
    expected = artifact.inputs(ROOT)
    pr = None if production else current_pr()
    # Production and engine-changing PRs need the build of exactly these inputs.
    match_inputs = production or engine_changed(pr)
    deadline = time.monotonic() + wait
    read_deadline = deadline if wait > 0 else None
    poll_delay = 2
    checked = set()
    keys = {}
    # The newest Bevy run of this commit and why it cannot supply the build: named for a rerun.
    head = pr['head'] if pr else os.environ.get('GITHUB_SHA')
    newest, hint = 0, None
    # History is read newest first, and only as far as the search needs it.
    history = workflow_runs(read_deadline)
    candidates, swept, pending = history, False, set()
    while True:
        seen, pending, paused, expired = pending, set(), False, False
        try:
            for run in candidates:
                # GitHub reuses the run ID on rerun; only this attempt was checked.
                attempt = (run['id'], run['run_attempt'])
                if not trusted(run, pr) or attempt in checked:
                    continue
                failed = run['status'] == 'completed' and run['conclusion'] != 'success'
                if match_inputs and run['head_sha'] == head and run['id'] >= newest:
                    # A rerun or a newer run of the commit, under way or succeeded, replaces an older failure.
                    newest = run['id']
                    hint = (f'The Bevy CI run of this commit did not succeed: {run["id"]} ({run["conclusion"]}). '
                            'Rerun it, then this job. ') if failed else None
                if failed:
                    checked.add(attempt)
                    continue
                revision = pr['merge'] if run['event'] == 'pull_request' else run['head_sha']
                if revision not in keys:
                    keys[revision] = inputs_at(revision)
                if keys[revision] is None or (match_inputs and keys[revision] != expected):
                    checked.add(attempt)
                    continue
                # The tree's own inputs are verified as such; another main build (a change
                # that touches no engine input) against the commit that built it.
                producer = None if keys[revision] == expected else revision
                try:
                    if run['status'] != 'completed':
                        if not production or receipt is not None:
                            result = download_run(run, pr, production, receipt, read_deadline, producer=producer)
                            if result:
                                print(f'Preparation import: web artifact uploaded in Bevy run {run["id"]}; '
                                      'remaining CI jobs are still running.', flush=True)
                                return result
                        pending.add(run['id'])
                        if candidates is history and match_inputs and time.monotonic() < deadline:
                            # This run builds exactly these inputs, so poll it now. Each older run costs
                            # a read and a shallow fetch; they are read only if this one gives no build.
                            paused = True
                            break
                        continue
                    checked.add(attempt)
                    if run['conclusion'] == 'success':
                        result = download_run(run, pr, production, receipt, read_deadline, producer=producer)
                        if result:
                            return result
                        if run['id'] == newest:
                            hint = ('The Bevy CI run of this commit succeeded, but its web artifact is '
                                    f'unavailable: {run["id"]}. Rerun it, then this job. ')
                except ProducerEnded as ended:
                    # A consumer job of the same run waits here: without this the wait lasts to the deadline.
                    checked.add(attempt)
                    if run['id'] == newest and str(ended) == 'skipped':
                        # The run decided that its engine inputs did not change: a rerun skips again.
                        hint = f'The Bevy CI run of this commit skipped the web build: {run["id"]}. '
                    elif run['id'] == newest:
                        hint = (f'The Bevy CI run of this commit did not upload a web build: {run["id"]} ({ended}). '
                                'Rerun it, then this job. ')
                except ValueError as exc:
                    if producer is None:
                        raise  # the build of these very inputs is corrupt or tampered with
                    # Usually main moved the HUD contract after this build and an older build
                    # still fits; a digest or archive error on a trusted artifact must be seen.
                    print(f'::warning::Skipping Bevy run {run["id"]} ({revision[:8]}): {exc}', flush=True)
                    checked.add(attempt)
        except TimeoutError:
            # The wait ended inside a read: name the runs last seen under way.
            pending |= seen
            expired = True
        if candidates is history and not paused and not expired:
            swept = True
        if not pending and not swept and not expired:
            # The run under way gave no build: go on with the older runs.
            candidates = history
            continue
        if not pending or expired or time.monotonic() >= deadline:
            recovery = hint or ''
            if pending:
                state = 'has not passed' if production and receipt is None else 'has not uploaded its web build'
                limit = f'after the {wait} s wait' if wait > 0 else 'and this import does not wait'
                recovery += (f'Bevy CI run {", ".join(map(str, sorted(pending)))} {state} {limit}. '
                             'Rerun this job when it has. ')
            else:
                recovery += ('Refresh main with: gh workflow run bevy-explorer-ci.yml --ref main. '
                             'Wait for its successful web build, then retry with matching inputs. ')
            if not production:
                recovery += ('For local development, build with make bevy-web and use '
                             '--from-build apps/bevy-explorer/dist/web. ')
            raise RuntimeError('No successful matching dcl-one Bevy CI artifact. ' + recovery +
                               'Sites will not load a different engine.')
        gate = 'complete Bevy CI run' if production and receipt is None else 'Bevy web producer'
        wanted = f'with inputs {expected[:12]}' if match_inputs else 'on main (this PR changes no engine input)'
        print(f'Waiting for a successful {gate} {wanted} '
              f'({"matching run in progress" if pending else "no matching uploaded producer"})', flush=True)
        time.sleep(min(poll_delay, max(0, deadline - time.monotonic())))
        poll_delay = min(poll_delay * 2, 30)
        candidates = poll_runs(pending, read_deadline)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--from-build', type=Path, help='Import a verified local build instead of a CI artifact')
    parser.add_argument('--wait', type=int, default=int(os.environ.get('FOUNDATION_PLAY_WAIT_SECONDS', '0')))
    mode.add_argument('--production', action='store_true', help='Revalidate and import optimized trusted-main output')
    mode.add_argument('--prepare-production', type=Path, metavar='RECEIPT',
                      help='Prepare optimized trusted-main output before full CI; write a publication receipt')
    mode.add_argument('--verify-production', type=Path, metavar='RECEIPT',
                      help='Require complete CI for the exact artifact in a trusted build receipt')
    mode.add_argument('--print-directory', action='store_true')
    args = parser.parse_args(argv)
    if args.verify_production:
        verify_production(args.verify_production, args.wait)
        return 0
    if args.prepare_production:
        # Never leave a receipt from an earlier successful import on failure.
        args.prepare_production.unlink(missing_ok=True)
        fetch(args.wait, production=True, receipt=args.prepare_production)
        return 0
    current = ROOT / '.cache/play/current'
    if args.print_directory:
        print(current.resolve(strict=True))
        return 0
    if args.from_build:
        install(args.from_build)
        return 0
    # A local manifest proves byte integrity, not a successful trusted-main run.
    # Production always rechecks GitHub and its archive digest, even on cache hits.
    if not args.production:
        try:
            trust = (current / artifact.PRODUCER_VALIDATION).is_file()
            artifact.verify(current, ROOT, trust_inputs=trust)
            print(f'Sites Explorer already verified at {current}')
            return 0
        except (OSError, ValueError, KeyError):
            pass
    fetch(args.wait, args.production)
    return 0


if __name__ == '__main__':
    main()
