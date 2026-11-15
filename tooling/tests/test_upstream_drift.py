"""The pinned source list, the upstream check, the drift collector's parsing and version logic,
and the site's markdown rendering. Nothing here reaches the network."""
import importlib.util
from pathlib import Path
import re
import tempfile
import textwrap
import unittest
from unittest import mock

TOOLING = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('upstream_drift', TOOLING / 'upstream-drift.py')
drift = importlib.util.module_from_spec(spec)
spec.loader.exec_module(drift)
spec2 = importlib.util.spec_from_file_location('upstream_drift_site', TOOLING / 'upstream-drift-site.py')
site = importlib.util.module_from_spec(spec2)
spec2.loader.exec_module(site)


SOURCE_TOML = textwrap.dedent('''
    name       = "winit"
    upstream   = "https://github.com/rust-windowing/winit"
    commit     = "1111111111111111111111111111111111111111"
    ref        = "v0.30.12"
    origin     = "https://github.com/example/winit"
    origin_rev = "2222222222222222222222222222222222222222"
    license    = "Apache-2.0"
    paths      = ["."]
    exclude    = []
    base_tree  = "3333333333333333333333333333333333333333"
    tree       = "4444444444444444444444444444444444444444"
    nix_hash   = ""
''')

# What `tooling/patched-sources.py export` writes: no upstream commit, so the all-zero one.
LOCAL_PATCH = textwrap.dedent('''\
    From 0000000000000000000000000000000000000000
    Subject: resolve the merge by hand
    Date: 2026-03-05T10:00:00+01:00

    ---
    diff --git a/src/lib.rs b/src/lib.rs
''')

PATCH = textwrap.dedent('''\
    From 5555555555555555555555555555555555555555
    Subject: keep the cursor grab on focus loss
    Origin: https://github.com/example/winit/commit/5555555555555555555555555555555555555555
    Date: 2026-03-04T10:00:00+01:00

    The body may name a line that looks like a header.
    Subject: not this one
    ---
    diff --git a/src/lib.rs b/src/lib.rs
''')

LOCK = textwrap.dedent('''
    [[package]]
    name = "serde"
    version = "1.0.0"
    source = "registry+https://github.com/rust-lang/crates.io-index"

    [[package]]
    name = "gltf-json"
    version = "1.4.1"
    source = "git+https://github.com/gltf-rs/gltf?rev=12fc1b7c#12fc1b7ce28c7b4c6d1b881a30df24060dccfbda"

    [[package]]
    name = "gltf"
    version = "1.4.1"
    source = "git+https://github.com/gltf-rs/gltf?rev=12fc1b7c#12fc1b7ce28c7b4c6d1b881a30df24060dccfbda"

    [[package]]
    name = "draco-oxide-core"
    version = "0.2.0"
    source = "git+https://github.com/example/draco-oxide?branch=fix%2Fwasm32-rans-refill#5acfcf6"

    [[package]]
    name = "local"
    version = "0.1.0"
''')

MANIFEST = textwrap.dedent('''
    [dependencies]
    # Pin the merge commit
    # until a release ships it.
    gltf = { git = "https://github.com/gltf-rs/gltf", rev = "12fc1b7c" }
    gltf-extra = { git = "https://github.com/gltf-rs/gltf-extra" }
''')

GLTF = 'https://github.com/gltf-rs/gltf'


def pin_tree(root):
    """A pinned Explorer in the contract's layout: one patched source and a lock with git sources."""
    series = root / 'patches/winit/series'
    series.mkdir(parents=True)
    (root / 'patches/winit/source.toml').write_text(SOURCE_TOML)
    (series / '0001-keep-the-cursor-grab.patch').write_text(PATCH)
    (series / '0002-merge-resolution.patch').write_text(LOCAL_PATCH)
    (series / '0003-no-header.patch').write_text('---\ndiff --git a/x b/x\n')
    (root / 'source').mkdir()
    (root / 'source/Cargo.lock').write_text(LOCK)
    (root / 'source/Cargo.toml').write_text(MANIFEST)
    return root


class PinnedSourceTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.pin = pin_tree(Path(directory.name))

    def test_a_patched_source_is_its_upstream_commit_ref_and_series(self):
        winit, = drift.patched_sources(self.pin / 'patches')
        self.assertEqual({k: v for k, v in winit.items() if k != 'patches'},
                         {'name': 'winit', 'upstream': 'https://github.com/rust-windowing/winit',
                          'commit': '1' * 40, 'ref': 'v0.30.12'})
        self.assertEqual(winit['patches'], [
            {'sha': '5' * 40, 'local': False, 'url': 'https://github.com/example/winit/commit/' + '5' * 40,
             'file': '0001-keep-the-cursor-grab.patch', 'date': '2026-03-04', 'title': 'keep the cursor grab on focus loss',
             'author': 'example/winit'},
            {'sha': '', 'local': True, 'url': '', 'file': '0002-merge-resolution.patch', 'date': '2026-03-05',
             'title': 'resolve the merge by hand', 'author': ''},
            {'sha': '', 'local': True, 'url': '', 'file': '0003-no-header.patch', 'date': '', 'title': '0003-no-header',
             'author': ''}])

    def test_every_text_file_is_read_and_written_as_utf_8(self):
        # The default encoding is the locale's, and a patch subject or a commit title is not ASCII.
        for tool in ('upstream-drift.py', 'upstream-drift-site.py'):
            text = (TOOLING / tool).read_text(encoding='utf-8')
            self.assertEqual(re.findall(r'\b(?:read|write)_text\((?![^\n]*encoding=)[^\n]*', text), [], tool)

    def test_lock_git_sources_are_grouped_and_name_the_ref_to_watch(self):
        draco, gltf = drift.lock_git_sources(LOCK)
        self.assertEqual(draco, {'name': 'draco-oxide', 'upstream': 'https://github.com/example/draco-oxide', 'commit': '5acfcf6',
                                 'ref': 'fix/wasm32-rans-refill', 'crates': [('draco-oxide-core', '0.2.0')]})
        self.assertEqual(gltf, {'name': 'gltf', 'upstream': GLTF, 'commit': '12fc1b7ce28c7b4c6d1b881a30df24060dccfbda',
                                'ref': None, 'crates': [('gltf', '1.4.1'), ('gltf-json', '1.4.1')]})

    def test_reasons_come_from_the_comment_block_above_the_first_mention(self):
        reasons = drift.harvest_reasons(MANIFEST, {GLTF, GLTF + '-extra'})
        self.assertEqual(reasons, {GLTF: 'Pin the merge commit until a release ships it.'})

    def test_pinned_sources_are_the_patched_ones_then_the_lock_with_reasons(self):
        sources = drift.pinned_sources(self.pin)
        self.assertEqual([s['name'] for s in sources], ['winit', 'draco-oxide', 'gltf'])
        self.assertEqual(sources[2]['reason'], 'Pin the merge commit until a release ships it.')
        self.assertIsNone(sources[1]['reason'])

    def test_the_tracked_sources_follow_the_contract(self):
        sources = drift.patched_sources(drift.PIN / 'patches')
        self.assertIn('bevy', [s['name'] for s in sources])
        for source in sources:
            self.assertRegex(source['commit'], r'^[0-9a-f]{40}$')
            self.assertIsNotNone(drift.github_repo(source['upstream']), source['upstream'])
            self.assertNotIn('crates', source)
            # The check proves a pin against the ref its source names: a source with none proves nothing.
            self.assertTrue(source['ref'], source['name'])
            for patch in source['patches']:
                self.assertTrue(patch['title'], patch['file'])
                # A patch is a commit of a public repository, with its address, or a local patch.
                self.assertEqual(patch['local'], not patch['sha'], patch['file'])
                self.assertEqual(bool(patch['url']), not patch['local'], patch['file'])
                self.assertNotRegex(patch['sha'], r'^0+$', patch['file'])


def fake_api(answers):
    def api(path):
        if path not in answers:
            raise AssertionError(path)
        return answers[path]
    return api


def tag_rows(*names):
    return [{'ref': f'refs/tags/{name}'} for name in names]


WINIT = 'repos/rust-windowing/winit'
WINIT_TAGS = WINIT + '/git/matching-refs/tags/v'


class CheckTests(unittest.TestCase):
    def test_a_tag_that_stayed_reports_the_newer_tags_of_its_scheme(self):
        source = {'name': 'winit', 'upstream': 'https://github.com/rust-windowing/winit', 'commit': 'a' * 40, 'ref': 'v0.30.12'}
        answers = {WINIT + '/compare/' + 'a' * 40 + '...v0.30.12': {'ahead_by': 0, 'behind_by': 0},
                   WINIT_TAGS: tag_rows('v0.29.0', 'v0.30.12', 'v0.30.13', 'v0.31.0-beta.2', 'v0.31.0', 'v0.30.9', 'vello-1.0')}
        result = drift.check_source(source, fake_api(answers))
        self.assertEqual(result, {'ref': 'v0.30.12', 'ahead': 0, 'behind': 0, 'named': True, 'tag': True,
                                  'newer_tags': ['v0.30.13', 'v0.31.0']})
        self.assertIsNone(drift.off_ref(result))
        answers[WINIT_TAGS] = tag_rows('v0.29.0', 'v0.30.12')
        self.assertEqual(drift.check_source(source, fake_api(answers))['newer_tags'], [])

    def test_a_pre_release_pin_counts_pre_releases_and_a_release_pin_does_not(self):
        tags = ['v0.1.0-alpha.9', 'v0.1.0-alpha.10', 'v0.1.0-rc.1', 'v0.1.0', 'v0.2.0-alpha.1']
        self.assertEqual(drift.newer_tags(tags, 'v0.1.0-alpha.9'), ['v0.1.0-alpha.10', 'v0.1.0-rc.1', 'v0.1.0', 'v0.2.0-alpha.1'])
        self.assertEqual(drift.newer_tags(tags, 'v0.1.0'), [])
        self.assertEqual(drift.newer_tags(['0.18', '0.18.1', '0.19.0-dev', '0.17.3'], '0.18'), ['0.18.1'])

    def test_tags_of_another_scheme_are_not_the_scheme_of_the_ref(self):
        answers = {'repos/livekit/rust-sdks/git/matching-refs/tags/livekit/v':
                   tag_rows('livekit/v0.7.1', 'livekit/v0.7.2', 'livekit/v-odd/v1.0.0')}
        self.assertEqual(drift.scheme_tags('livekit/rust-sdks', 'livekit/v0.7.1', fake_api(answers)), ['livekit/v0.7.1', 'livekit/v0.7.2'])

    def test_a_tag_that_no_longer_names_the_pin_is_off_its_ref(self):
        source = {'name': 'winit', 'upstream': 'https://github.com/rust-windowing/winit', 'commit': 'a' * 40, 'ref': 'v0.30.12'}
        answers = {WINIT + '/compare/' + 'a' * 40 + '...v0.30.12': {'ahead_by': 2, 'behind_by': 0}, WINIT_TAGS: tag_rows('v0.30.12')}
        self.assertEqual(drift.off_ref(drift.check_source(source, fake_api(answers))),
                         'the tag moved, or never named the pin: it is 2 commit(s) past it')

    def test_a_branch_that_moved_counts_its_commits(self):
        source = {'name': 'draco-oxide', 'upstream': 'https://github.com/example/draco-oxide', 'commit': 'b' * 40, 'ref': 'fix/rans'}
        answers = {'repos/example/draco-oxide/compare/' + 'b' * 40 + '...fix%2Frans': {'ahead_by': 4, 'behind_by': 0},
                   'repos/example/draco-oxide/git/matching-refs/tags/fix/rans': []}
        result = drift.check_source(source, fake_api(answers))
        self.assertEqual(result, {'ref': 'fix/rans', 'ahead': 4, 'behind': 0, 'named': True, 'tag': False, 'newer_tags': []})
        self.assertIsNone(drift.off_ref(result))

    def test_a_pin_the_named_branch_lacks_is_off_its_ref(self):
        source = {'name': 'draco-oxide', 'upstream': 'https://github.com/example/draco-oxide', 'commit': 'b' * 40, 'ref': 'main'}
        answers = {'repos/example/draco-oxide/compare/' + 'b' * 40 + '...main': {'ahead_by': 4, 'behind_by': 1},
                   'repos/example/draco-oxide/git/matching-refs/tags/main': tag_rows('maintenance-1')}
        self.assertEqual(drift.off_ref(drift.check_source(source, fake_api(answers))), 'the pin has 1 commit(s) main lacks')

    def test_a_source_pinned_by_revision_alone_watches_the_default_branch_and_claims_nothing(self):
        source = {'name': 'gltf', 'upstream': GLTF, 'commit': 'c' * 40, 'ref': None}
        answers = {'repos/gltf-rs/gltf': {'default_branch': 'main'},
                   'repos/gltf-rs/gltf/compare/' + 'c' * 40 + '...main': {'ahead_by': 26, 'behind_by': 2}}
        result = drift.check_source(source, fake_api(answers))
        self.assertEqual(result, {'ref': 'main', 'ahead': 26, 'behind': 2, 'named': False, 'tag': False, 'newer_tags': []})
        self.assertIsNone(drift.off_ref(result))

    def test_a_source_github_cannot_compare_is_not_checked(self):
        gone = {'name': 'gone', 'upstream': 'https://github.com/example/gone', 'commit': 'd' * 40, 'ref': 'v1'}
        self.assertIsNone(drift.check_source(gone, fake_api({'repos/example/gone/compare/' + 'd' * 40 + '...v1': None})))
        elsewhere = {'name': 'x', 'upstream': 'https://example.org/x.git', 'commit': 'e' * 40, 'ref': 'main'}
        self.assertIsNone(drift.check_source(elsewhere, fake_api({})))

    def report(self, behind=0):
        sources = [
            {'name': 'winit', 'upstream': 'https://github.com/rust-windowing/winit', 'commit': 'a' * 40, 'ref': 'v0.30.12',
             'patches': [{'local': False}, {'local': True}]},
            {'name': 'gltf', 'upstream': GLTF, 'commit': 'c' * 40, 'ref': None},
        ]
        answers = {WINIT + '/compare/' + 'a' * 40 + '...v0.30.12': {'ahead_by': 0, 'behind_by': behind},
                   WINIT_TAGS: tag_rows('v0.30.12', 'v0.30.13', 'v0.31.0'),
                   'repos/gltf-rs/gltf': {'default_branch': 'main'},
                   'repos/gltf-rs/gltf/compare/' + 'c' * 40 + '...main': {'ahead_by': 26, 'behind_by': 2}}
        return sources, answers

    def test_the_report_has_one_line_per_source_with_local_patches_and_tag_drift(self):
        sources, answers = self.report()
        text, ok = drift.check_report(sources, fake_api(answers))
        self.assertTrue(ok)
        self.assertEqual(text.splitlines(), [
            'Pinned sources of the Explorer: 2; upstream moved past the pin: 1; a newer tag: 1; pin not on its ref: 0; not checked: 0.',
            '',
            '- winit: rust-windowing/winit tag v0.30.12 has not moved past aaaaaaaaaa; 2 patch(es), 1 local; 2 newer tag(s), newest v0.31.0',
            '- gltf: gltf-rs/gltf main is 26 commit(s) past cccccccccc (the pin has 2 commit(s) the ref lacks)'])

    def test_the_report_fails_on_a_pin_that_is_not_on_its_ref(self):
        sources, answers = self.report(behind=3)
        text, ok = drift.check_report(sources, fake_api(answers))
        self.assertFalse(ok)
        self.assertIn('pin not on its ref: 1; not checked: 0.', text)
        self.assertIn('- winit: rust-windowing/winit tag v0.30.12 has not moved past aaaaaaaaaa; '
                      'PIN NOT ON ITS REF: the pin has 3 commit(s) v0.30.12 lacks; 2 patch(es), 1 local', text)

    def test_the_report_fails_on_an_unchecked_source(self):
        sources, answers = self.report()
        sources.append({'name': 'x', 'upstream': 'https://example.org/x.git', 'commit': 'e' * 40, 'ref': 'main'})
        text, ok = drift.check_report(sources, fake_api(answers))
        self.assertFalse(ok)
        self.assertIn('not checked: 1.', text)
        self.assertEqual(text.splitlines()[-1], '- x: https://example.org/x.git NOT CHECKED: not a GitHub repository')

    def test_the_command_checks_the_named_sources_and_rejects_an_unknown_name(self):
        sources = [{'name': 'winit'}, {'name': 'gltf'}]
        seen = []
        with mock.patch.object(drift, 'pinned_sources', return_value=sources), \
                mock.patch.object(drift, 'check_report', side_effect=lambda chosen: (seen.append(chosen) or ('', False))), \
                mock.patch.object(drift.sys, 'argv', ['upstream-drift.py', 'check', 'gltf']):
            self.assertEqual(drift.main(), 1)
        self.assertEqual(seen, [[{'name': 'gltf'}]])
        with mock.patch.object(drift, 'pinned_sources', return_value=sources), mock.patch.object(drift.sys, 'stderr'), \
                mock.patch.object(drift.sys, 'argv', ['upstream-drift.py', 'check', 'winnit']), self.assertRaises(SystemExit) as stop:
            drift.main()
        self.assertEqual(stop.exception.code, 2)


class CollectTests(unittest.TestCase):
    def test_a_source_entry_has_the_shape_the_site_reads(self):
        commit = {'sha': 'f' * 40, 'commit': {'committer': {'date': '2026-05-01T00:00:00Z'}, 'author': {'name': 'someone'}, 'message': 'a change\n\nbody'}}
        answers = {
            'repos/rust-windowing/winit': {'description': 'windows', 'stargazers_count': 5, 'archived': False, 'default_branch': 'master'},
            'repos/rust-windowing/winit/commits/' + 'a' * 40: {'commit': {'committer': {'date': '2026-01-02T00:00:00Z'}}},
            'repos/rust-windowing/winit/compare/' + 'a' * 40 + '...master?per_page=250&page=1':
                {'status': 'ahead', 'ahead_by': 1, 'behind_by': 0, 'total_commits': 1, 'commits': [commit]},
            WINIT_TAGS: tag_rows('v0.30.11', 'v0.30.12', 'v0.31.0'),
            'repos/rust-windowing/winit/compare/' + 'a' * 40 + '...v0.30.12?per_page=250&page=1':
                {'status': 'identical', 'ahead_by': 0, 'behind_by': 0, 'total_commits': 0, 'commits': []},
            'repos/rust-windowing/winit/releases?per_page=100&page=1': [
                {'tag_name': 'v0.31.0', 'published_at': '2026-04-01T00:00:00Z', 'html_url': 'https://example.org/r'},
                {'tag_name': 'v0.30.12', 'published_at': '2026-01-03T00:00:00Z', 'html_url': 'https://example.org/q'}],
            'repos/rust-windowing/winit/releases/latest': {'tag_name': 'v0.31.0', 'published_at': '2026-04-01T00:00:00Z', 'html_url': 'https://example.org/r'},
        }
        patch = {'sha': '5' * 40, 'local': False, 'url': 'https://github.com/example/winit/commit/' + '5' * 40,
                 'file': '0001-keep.patch', 'date': '2026-03-04', 'title': 'keep the cursor grab', 'author': 'example/winit'}
        source = {'name': 'winit', 'upstream': 'https://github.com/rust-windowing/winit', 'commit': 'a' * 40, 'ref': 'v0.30.12', 'patches': [patch]}
        with mock.patch.object(drift, 'gh', fake_api(answers)), mock.patch.object(drift.sys, 'stderr'):
            entry, = drift.collect_sources([source])
        self.assertEqual((entry['url'], entry['repo'], entry['rev'], entry['branch']),
                         ('https://github.com/rust-windowing/winit', 'rust-windowing/winit', 'a' * 40, 'v0.30.12'))
        self.assertNotIn('crates', entry)
        self.assertEqual(entry['carried'], {'commits': [patch]})
        self.assertEqual(entry['base'], {'tag': 'v0.30.12', 'sha': 'a' * 40, 'date': '2026-01-02'})
        self.assertEqual(entry['newer_tags'], ['v0.31.0'])
        self.assertEqual(entry['upstream_since_pin']['total_commits'], 1)
        self.assertEqual(entry['upstream_since_pin']['commits'][0]['title'], 'a change')
        self.assertEqual(entry['branch_since_pin']['total_commits'], 0)
        self.assertEqual([r['tag'] for r in entry['releases_since_base']], ['v0.31.0'])
        self.assertEqual(entry['parent_latest_release']['tag'], 'v0.31.0')
        # The page reads no key the collector does not write; `crates` is the key of a lock source.
        script = site.PAGE[site.PAGE.index('<script>'):]
        read = set(re.findall(r'\bs\.([a-z_]+)', script))
        self.assertEqual(read - set(entry), {'crates'})
        self.assertEqual(set(re.findall(r'\bs\.carried\.([a-z_]+)', script)), set(entry['carried']))
        self.assertEqual(set(re.findall(r'\bc\.(sha|local|url|file|date|author|title)\b', script)) - set(patch), set())
        for gone in ('parent_info', 'vs_parent_default', 'config_pin', 'base_note', 'upstream_since_base', 'fork_only'):
            self.assertNotIn(gone, site.PAGE)

    def test_a_lock_source_has_crates_and_no_patch_series(self):
        answers = {'repos/gltf-rs/gltf': {'description': 'gltf', 'stargazers_count': 1, 'archived': False, 'default_branch': 'main'},
                   'repos/gltf-rs/gltf/commits/' + 'c' * 40: {'commit': {'committer': {'date': '2026-01-02T00:00:00Z'}}},
                   'repos/gltf-rs/gltf/compare/' + 'c' * 40 + '...main?per_page=250&page=1':
                       {'status': 'identical', 'ahead_by': 0, 'behind_by': 0, 'total_commits': 0, 'commits': []},
                   'repos/gltf-rs/gltf/releases?per_page=100&page=1': [],
                   'repos/gltf-rs/gltf/releases/latest': None}
        source = {'name': 'gltf', 'upstream': GLTF, 'commit': 'c' * 40, 'ref': None, 'reason': None, 'crates': [('gltf', '1.4.1')]}
        with mock.patch.object(drift, 'gh', fake_api(answers)), mock.patch.object(drift.sys, 'stderr'):
            entry, = drift.collect_sources([source])
        self.assertEqual(entry['crates'], [{'name': 'gltf', 'version': '1.4.1'}])
        for absent in ('carried', 'base', 'newer_tags', 'branch_since_pin'):
            self.assertNotIn(absent, entry)


class VersionLogicTests(unittest.TestCase):
    def test_requirements_follow_cargo_semantics(self):
        self.assertTrue(drift.matches_req('0.23.37', '>= 0.23.0'))
        self.assertFalse(drift.matches_req('0.23.37', '>= 0.23.38'))
        self.assertTrue(drift.matches_req('1.2.3', '^1.0.0'))
        self.assertFalse(drift.matches_req('2.0.0', '^1.0.0'))
        self.assertTrue(drift.matches_req('0.3.3', '^0.3.1'))
        self.assertFalse(drift.matches_req('0.4.0', '^0.3.1'))
        self.assertTrue(drift.matches_req('0.9.18', '>= 0.9.0, < 0.9.19'))
        self.assertFalse(drift.matches_req('0.9.19', '>= 0.9.0, < 0.9.19'))

    def test_affected_means_neither_patched_nor_unaffected(self):
        advisory = {'patched': ['>= 0.23.40'], 'unaffected': ['< 0.20.0']}
        self.assertTrue(drift.affected('0.23.37', advisory))
        self.assertFalse(drift.affected('0.23.40', advisory))
        self.assertFalse(drift.affected('0.19.0', advisory))

    def test_advisory_markdown_parses_front_matter_and_title(self):
        text = textwrap.dedent('''
            ```toml
            [advisory]
            id = "RUSTSEC-2026-0001"
            package = "demo"
            date = "2026-01-02"
            aliases = ["CVE-2026-1"]
            informational = "unmaintained"

            [versions]
            patched = []
            unaffected = ["< 0.1.0"]
            ```

            # demo is unmaintained

            Body text.
        ''')
        advisory = drift.parse_advisory(text)
        self.assertEqual(advisory['id'], 'RUSTSEC-2026-0001')
        self.assertEqual(advisory['title'], 'demo is unmaintained')
        self.assertEqual(advisory['informational'], 'unmaintained')
        self.assertEqual(advisory['unaffected'], ['< 0.1.0'])
        self.assertIsNone(drift.parse_advisory('no toml here'))

    def test_tag_helpers(self):
        self.assertEqual(drift.version_key('v0.16.1'), (0, 16, 1, 0))
        self.assertGreater(drift.version_key('v1.46.3'), drift.version_key('v0.28.0'))
        self.assertEqual(drift.tag_prefix('livekit-ffi/v0.12.83'), 'livekit-ffi/v')
        self.assertEqual(drift.tag_prefix('0.307.0'), '')
        ordered = ['v0.9.9', 'v0.10.0-alpha.9', 'v0.10.0-alpha.10', 'v0.10.0-beta.1', 'v0.10.0-rc.2', 'v0.10.0', 'v0.10.1']
        self.assertEqual(sorted(reversed(ordered), key=drift.tag_order), ordered)

    def test_lock_packages_and_shape(self):
        lock = textwrap.dedent('''
            [[package]]
            name = "a"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "a"
            version = "2.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "b"
            version = "0.1.0"
            source = "git+https://github.com/x/b?branch=main#abc"

            [[package]]
            name = "ws"
            version = "0.0.0"
        ''')
        pkgs = drift.lock_packages(lock)
        shape = drift.lock_shape(pkgs)
        self.assertEqual(shape['packages'], 4)
        self.assertEqual(shape['by_source'], {'crates.io': 2, 'git': 1, 'workspace': 1})
        self.assertEqual(shape['duplicates'], [{'name': 'a', 'versions': ['1.0.0', '2.0.0']}])


class SiteTests(unittest.TestCase):
    def test_markdown_covers_the_review_constructs(self):
        html = site.markdown_to_html('# Title\n\nA **bold** `code` [link](https://x.y) line.\n\n- one\n- two\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n')
        self.assertIn('<h1 id="title">Title</h1>', html)
        self.assertIn('<strong>bold</strong> <code>code</code> <a href="https://x.y"', html)
        self.assertIn('<ul>\n<li>one</li>\n<li>two</li>\n</ul>', html)
        self.assertIn('<table><thead><tr><th>a</th><th>b</th></tr></thead><tbody><tr><td>1</td><td>2</td></tr></tbody></table>', html)

    def test_slug(self):
        self.assertEqual(site.slug('https://github.com/robtfm/bevy_kira_audio'), 'robtfm-bevy-kira-audio')


if __name__ == '__main__':
    unittest.main()
