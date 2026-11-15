#!/usr/bin/env python3
"""Check mechanically detectable violations in changed web UI files.

Best effort: a pass means no CSS-in-JS import or new same-name component
stylesheet was detected in the changed files, not that they follow every rule
in docs/ui-standards.md.

`--tokens PATH...` is a report instead of a check (exit 0): for every stylesheet
and CSS Module under the given paths it counts the raw colour literals (hex,
rgb/rgba, hsl/hsla and named colours other than transparent, currentColor and
inherit), the px lengths equal to a `--dcl-space-*` or `--dcl-radius-*` token,
the `font-family` declarations, the `var(--dcl-` uses and the `no token` drift
comments, then prints the totals. It measures how far a surface has moved onto
the shared foundations (docs/ui-standards.md, "Verification").

`--token-floor` is a check: for every stylesheet under FLOOR_PATHS it fails when
the `var(--dcl-` uses drop below, or the `var()` uses of an apps/ui3
atoms/primitives.css property rise above, what tooling/ui-token-floor.json
records (a stylesheet it does not list may use no primitives property), and
when the file records a stylesheet that is not there, so a move or a removal
is a reviewed change of the floor. `--write-token-floor` records the current
counts after a deliberate change; it leaves out a stylesheet with no use of
either kind, which is the floor of an unlisted one.
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path, PurePosixPath

ROOT = Path(__file__).resolve().parent.parent
SCRIPT_EXTENSIONS = ('.ts', '.tsx', '.js', '.jsx', '.mjs', '.cjs')
EXTENSIONS = SCRIPT_EXTENSIONS + ('.css',)
COMPONENT_EXTENSIONS = ('.tsx', '.jsx')
UI_ROOTS = ('apps/', 'library/ui/', 'rendering/bevy-explorer/source/react-web/')
# Under apps/ but not web UI: the scene SDK toolchain's scripts, scene templates and fixtures.
NON_WEB_ROOTS = ('apps/dcl-scene-cli/scripts/', 'apps/dcl-scene-cli/src/templates/', 'apps/dcl-scene-cli/testdata/')
# Application entry stylesheets (App.css, index.css, main.css directly in an app's src/) are global by design.
ENTRY_STYLESHEETS = ('app', 'index', 'main')
ENTRY_DIRECTORY = re.compile(r'^(?:apps/[^/]+/src|rendering/bevy-explorer/source/react-web/src)/[^/]+$')
# Files that already broke these rules when a whole project was imported (apps/creator-hub,
# docs/adr/2026-11-09-creator-hub-in-dcl-one.md). They pass only as the files that change adds;
# any later change to one is checked like every other file. The list is read from the base revision,
# so nothing can be added to it after the change that created it, and only that change is exempted
# (imported_paths).
IMPORTED = 'tooling/ui-standards-imported.txt'
ZERO_SHA = re.compile(r'^0+$')
COMMIT_SHA = re.compile(r'^[0-9a-f]{40}$')
CSS_IN_JS_MODULE = re.compile(r'^(?:styled-components|@emotion/(?:styled|react|css)|@mui/(?:material/styles|system)/styled|@mui/styled-engine(?:-sc)?)(?:/.*)?$')
MUI_MODULE = re.compile(r'^(?:@mui/material(?:/styles)?|@mui/system|decentraland-ui2)$')
TOKEN = re.compile(
    r"(?P<comment>//[^\n]*|/\*.*?(?:\*/|\Z))"
    r"|(?P<string>'(?:\\.|[^'\\\n])*'|\"(?:\\.|[^\"\\\n])*\"|`(?:\\.|[^`\\])*`)"
    r"|(?P<word>[A-Za-z_$][\w$]*)"
    r"|(?P<punct>\S)",
    re.S,
)


def is_ui_source(path):
    return path.endswith(EXTENSIONS) and path.startswith(UI_ROOTS) and not path.startswith(NON_WEB_ROOTS)


def tokens(text):
    """Split source into words, punctuation and string literals, dropping comments."""
    result = []
    for match in TOKEN.finditer(text):
        kind = match.lastgroup
        if kind == 'comment':
            continue
        value = match.group()
        result.append((kind, value[1:-1] if kind == 'string' else value))
    return result


STATEMENT_WORDS = {'const', 'let', 'var', 'function', 'class', 'return', 'if', 'for', 'while', 'switch',
                   'interface', 'enum', 'default', 'export', 'import', 'await', 'async', 'new'}
EMOTION_PRAGMA = re.compile(r'@jsxImportSource\s+@emotion/react\b')


def import_clause(items, start):
    """Read an import/export clause from items[start:] up to its specifier.

    Returns (names, specifier) or None when the statement is not a module import or re-export.
    Braces may span lines; outside them, a statement keyword ends the clause, so code without
    semicolons does not run into the next statement.
    """
    names = []
    depth = 0
    previous = None
    for kind, value in items[start:]:
        if kind == 'string':
            if previous == ('word', 'from') or (previous is None and not names):
                return names, value
            return None
        if kind == 'punct' and value == ';':
            return None
        if kind == 'punct' and value == '{':
            depth += 1
        elif kind == 'punct' and value == '}':
            depth -= 1
        elif kind == 'word':
            if depth == 0 and value in STATEMENT_WORDS and value != 'default':
                return None
            names.append(value)
        elif kind == 'punct' and value == '*':
            names.append(value)
        previous = (kind, value)
    return None


def destructured_before(items, index):
    """Names destructured on the left of `= [await] require(...)` / `= await import(...)`."""
    cursor = index - 1
    if cursor >= 0 and items[cursor] == ('word', 'await'):
        cursor -= 1
    if cursor < 0 or items[cursor] != ('punct', '='):
        return set()
    cursor -= 1
    if cursor < 0 or items[cursor] != ('punct', '}'):
        return set()
    names = set()
    while cursor >= 0 and items[cursor] != ('punct', '{'):
        if items[cursor][0] == 'word':
            names.add(items[cursor][1])
        cursor -= 1
    return names


def forbidden_imports(text):
    """Return the module specifiers of the CSS-in-JS imports in source text."""
    found = ['@emotion/react (jsxImportSource)'] if EMOTION_PRAGMA.search(text) else []
    items = tokens(text)
    namespaces = set()
    for index, (kind, value) in enumerate(items):
        if kind != 'word' or value not in ('import', 'export', 'require'):
            continue
        if index and items[index - 1] == ('punct', '.'):
            continue
        following = items[index + 1:index + 3]
        if following and following[0] == ('punct', '('):
            if len(following) > 1 and following[1][0] == 'string':
                specifier = following[1][1]
                if CSS_IN_JS_MODULE.match(specifier) or (
                        MUI_MODULE.match(specifier) and 'styled' in destructured_before(items, index)):
                    found.append(specifier)
            continue
        if value == 'require':
            continue
        clause = import_clause(items, index + 1)
        if clause is None:
            continue
        names, specifier = clause
        if CSS_IN_JS_MODULE.match(specifier) or ('styled' in names and MUI_MODULE.match(specifier)):
            found.append(specifier)
        elif MUI_MODULE.match(specifier) and names[:2] == ['*', 'as'] and len(names) > 2:
            namespaces.add(names[2])
    for namespace in sorted(namespaces):
        uses_styled = any(items[i][0] == 'word' and items[i][1] == namespace and items[i + 1] == ('punct', '.')
                          and items[i + 2] == ('word', 'styled') for i in range(len(items) - 2))
        if uses_styled:
            found.append(f'{namespace}.styled')
    return found


def mentions_css_in_js(text):
    """Whether added text could belong to a CSS-in-JS import, so the whole file needs a scan."""
    if EMOTION_PRAGMA.search(text):
        return True
    return any((kind == 'word' and value == 'styled')
               or (kind == 'string' and (CSS_IN_JS_MODULE.match(value) or MUI_MODULE.match(value)))
               for kind, value in tokens(text))


def is_component_stylesheet(path, paths):
    if not path.endswith('.css') or path.endswith('.module.css'):
        return False
    if PurePosixPath(path).stem.casefold() in ENTRY_STYLESHEETS and ENTRY_DIRECTORY.match(path):
        return False
    base = path[:-4].casefold()
    normalized = {name.casefold() for name in paths}
    return any(base + extension in normalized for extension in COMPONENT_EXTENSIONS)


def is_plain_stylesheet(path):
    return path.endswith('.css') and not path.endswith('.module.css')


def added_lines(diff):
    """Return the added lines of a unified diff, skipping the file headers."""
    lines = []
    in_hunk = False
    for line in diff.splitlines():
        if line.startswith('@@'):
            in_hunk = True
        elif line.startswith('diff --git'):
            in_hunk = False
        elif in_hunk and line.startswith('+'):
            lines.append(line[1:])
    return lines


def git(*args):
    return subprocess.run(['git', '-c', 'core.quotePath=false', *args], cwd=ROOT, check=True,
                          capture_output=True, text=True).stdout


def names(command, *args):
    return {name for name in git(*command, '-z', *args).split('\0') if name}


def new_files(diff, base):
    """Paths added since base, plus renames whose old path was not already a plain stylesheet."""
    fields = git(*diff, '--no-color', '--name-status', '-z', '-M', '--diff-filter=AR', '--end-of-options', base, '--').split('\0')
    result = set()
    index = 0
    while index < len(fields) and fields[index]:
        status = fields[index]
        if status.startswith('R'):
            old, new = fields[index + 1], fields[index + 2]
            moved = is_plain_stylesheet(old) and PurePosixPath(old).name == PurePosixPath(new).name
            if not moved:
                result.add(new)
            index += 3
        else:
            result.add(fields[index + 1])
            index += 2
    return result


def verify(revision):
    try:
        git('rev-parse', '--verify', '--quiet', '--end-of-options', f'{revision}^{{commit}}')
        return True
    except subprocess.CalledProcessError:
        return False


def default_base():
    """The merge base with origin/main, so local runs see committed branch changes as CI does."""
    try:
        return git('merge-base', 'HEAD', 'origin/main').strip()
    except subprocess.CalledProcessError:
        print('UI standards: origin/main is not available; checking uncommitted changes only (pass --base)',
              file=sys.stderr)
        return 'HEAD'


def resolve_base(base):
    """Resolve the base revision.

    A push event reports an all-zero SHA for a new ref, and a force push can
    report a commit that is no longer present; both fall back to HEAD~1, or to
    HEAD in a repository with a single commit. Any other unresolvable base is
    an error rather than a silent pass, and so is a shallow clone without the
    history to compare against.
    """
    if not ZERO_SHA.match(base):
        if verify(base):
            return base
        if not COMMIT_SHA.match(base):
            raise ValueError(f'unknown base revision {base!r}')
    if verify('HEAD~1'):
        fallback = 'HEAD~1'
    elif git('rev-parse', '--is-shallow-repository').strip() == 'true':
        raise ValueError(f'base {base!r} is not available in this shallow clone; fetch more history')
    else:
        fallback = 'HEAD'
    print(f'UI standards: base {base!r} is not a reachable commit; comparing with {fallback}', file=sys.stderr)
    return fallback


def parse_imported(text):
    if text is None:
        return None
    return frozenset(line.strip() for line in text.splitlines() if line.strip() and not line.startswith('#'))


def imported_paths(base_text, current_text):
    """The import list's exempt paths, and the issues a change to the list raises.

    Only the change that creates the list (none at base) exempts what it lists. Once the list
    exists, it exempts nothing: a listed path deleted and later re-added is checked like any new
    file. The list can't grow or be deleted afterwards, since deleting it would let the next change
    create it again.
    """
    base_paths = parse_imported(base_text)
    current_paths = parse_imported(current_text)
    if base_paths is None:
        return current_paths or frozenset(), []
    if current_paths is None:
        return frozenset(), [f'{IMPORTED}: the import list can\'t be deleted; empty it instead']
    if current_paths - base_paths:
        return frozenset(), [f'{IMPORTED}: paths can\'t be added to the import list']
    return frozenset(), []


def check(base='HEAD', staged=False):
    """List violations in changes from base to the working tree, or to the index when staged."""
    base = resolve_base(base)
    if staged:
        diff = ('diff', '--cached')
        tracked = names(('ls-files',))
        untracked = set()
    else:
        diff = ('diff',)
        tracked = names(('ls-files',), '-co', '--exclude-standard')
        untracked = names(('ls-files',), '--others', '--exclude-standard')

    def current(name):
        if staged:
            return git('show', f':{name}')
        return (ROOT / name).read_text(encoding='utf-8', errors='replace')

    def previous(name):
        try:
            return git('show', f'{base}:{name}')
        except subprocess.CalledProcessError:
            return ''

    def existing(read, name):
        try:
            return read(name)
        except (OSError, subprocess.CalledProcessError):
            return None

    def at_base(name):
        return git('show', f'{base}:{name}')

    changed = names(diff, '--name-only', '--diff-filter=ACMR', '--end-of-options', base, '--') | untracked
    added = new_files(diff, base) | untracked
    exempt, issues = imported_paths(existing(at_base, IMPORTED), existing(current, IMPORTED))
    imported = exempt & added
    for name in sorted(added - imported):
        if is_ui_source(name) and is_component_stylesheet(name, tracked):
            issues.append(f'{name}: new component styles must use .module.css')
    for name in sorted(changed - imported):
        if not is_ui_source(name) or not name.endswith(SCRIPT_EXTENSIONS):
            continue
        if name in untracked:
            new_imports = forbidden_imports(current(name))
        else:
            text = '\n'.join(added_lines(git(*diff, '--no-ext-diff', '--no-color', '--unified=0',
                                              '--end-of-options', base, '--', name)))
            if not mentions_css_in_js(text):
                continue
            new_imports = forbidden_imports(current(name))
            for existing in forbidden_imports(previous(name)):
                if existing in new_imports:
                    new_imports.remove(existing)
        if new_imports:
            issues.append(f'{name}: new CSS-in-JS imports are not allowed')
    return issues


TOKENS_CSS = ROOT / 'library' / 'ui' / 'foundations' / 'spacing' / 'spacing.css'
SKIPPED_DIRECTORIES = {'node_modules', 'build', 'dist', '.react-router'}
# The CSS named colours, less the keywords that are not a literal value (transparent, currentColor, inherit).
NAMED_COLOURS = frozenset("""
aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet brown burlywood
cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan darkblue darkcyan darkgoldenrod darkgray
darkgreen darkgrey darkkhaki darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen
darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue dimgray dimgrey dodgerblue
firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray green greenyellow grey honeydew
hotpink indianred indigo ivory khaki lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan
lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue lightslategray
lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon mediumaquamarine mediumblue
mediumorchid mediumpurple mediumseagreen mediumslateblue mediumspringgreen mediumturquoise mediumvioletred
midnightblue mintcream mistyrose moccasin navajowhite navy oldlace olive olivedrab orange orangered orchid
palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru pink plum powderblue purple
rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown seagreen seashell sienna silver skyblue
slateblue slategray slategrey snow springgreen steelblue tan teal thistle tomato turquoise violet wheat white
whitesmoke yellow yellowgreen
""".split())
CSS_COMMENT = re.compile(r'/\*.*?\*/', re.S)
# A declaration: a property (custom properties included) and its value, up to the next ; { or }.
DECLARATION = re.compile(r'(?<![\w-])((?:--)?[\w-]+)\s*:\s*([^;{}]*)')
QUOTED_OR_URL = re.compile(r'url\([^)]*\)|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'')
VAR_NAME = re.compile(r'var\(\s*--[\w-]+')
HEX_COLOUR = re.compile(r'#[0-9a-fA-F]{3,8}\b')
FUNCTION_COLOUR = re.compile(r'\b(?:rgba?|hsla?)\(')
WORD = re.compile(r'[A-Za-z]+')
PX_LENGTH = re.compile(r'(?<![\w.-])(\d+(?:\.\d+)?)px\b')
TOKEN_USE = re.compile(r'var\(\s*--dcl-')
TOKEN_LENGTH = re.compile(r'--dcl-(?:space|radius)-[\w-]+:\s*(\d+(?:\.\d+)?)px')
COLUMNS = ('colours', 'lengths', 'fonts', 'tokens', 'drift')
HEADINGS = ('colour literals', 'token px', 'font-family', 'var(--dcl-', 'no token')


def token_lengths(css=None):
    """The px values of the --dcl-space-* and --dcl-radius-* tokens, read from the foundations' spacing.css."""
    text = TOKENS_CSS.read_text(encoding='utf-8') if css is None else css
    return {float(value) for value in TOKEN_LENGTH.findall(text)}


def colour_literals(value):
    """Count the raw colours in a declaration value: hex, rgb()/hsl() calls and named colours."""
    value = VAR_NAME.sub('var(', QUOTED_OR_URL.sub(' ', value))
    named = sum(1 for word in WORD.findall(value) if word.lower() in NAMED_COLOURS)
    return len(HEX_COLOUR.findall(value)) + len(FUNCTION_COLOUR.findall(value)) + named


def stylesheet_report(text, lengths):
    """Per-stylesheet counts: colour literals, token-equal px lengths, font-family declarations, token uses, drift comments."""
    body = CSS_COMMENT.sub(' ', text)
    counts = {'colours': 0, 'lengths': 0, 'fonts': 0, 'tokens': len(TOKEN_USE.findall(body)),
              'drift': sum(1 for comment in CSS_COMMENT.findall(text) if 'no token' in comment.lower())}
    for prop, value in DECLARATION.findall(body):
        if prop == 'font-family':
            counts['fonts'] += 1
        counts['colours'] += colour_literals(value)
        counts['lengths'] += sum(1 for px in PX_LENGTH.findall(QUOTED_OR_URL.sub(' ', value)) if float(px) in lengths)
    return counts


def stylesheets(paths):
    """Every .css file under the given files and directories, in path order, skipping build output."""
    result = set()
    for given in paths:
        path = ROOT / given
        if path.is_file():
            if path.suffix == '.css':
                result.add(path)
            continue
        for file in path.rglob('*.css'):
            if not SKIPPED_DIRECTORIES.intersection(file.relative_to(path).parts):
                result.add(file)
    return sorted(result)


def relative(path):
    try:
        return path.relative_to(ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def tokens_report(paths, lengths=None):
    """The --tokens report as lines: one per stylesheet, then the totals."""
    lengths = token_lengths() if lengths is None else lengths
    rows = [(relative(file), stylesheet_report(file.read_text(encoding='utf-8', errors='replace'), lengths))
            for file in stylesheets(paths)]
    rows.append((f'total ({len(rows)} files)', {column: sum(counts[column] for _, counts in rows) for column in COLUMNS}))
    width = max(len(name) for name, _ in rows)
    lines = [' '.join([' ' * width] + [heading.rjust(15) for heading in HEADINGS]).rstrip()]
    for name, counts in rows:
        lines.append(' '.join([name.ljust(width)] + [str(counts[column]).rjust(15) for column in COLUMNS]))
    return lines


FLOOR_FILE = ROOT / 'tooling' / 'ui-token-floor.json'
PRIMITIVES_CSS = ROOT / 'apps' / 'ui3' / 'src' / 'atoms' / 'primitives.css'
# The parity pages, their chrome and the shared components: surfaces that have moved onto library/ui.
FLOOR_PATHS = ('apps/sites/src/features', 'apps/ui3/src/web', 'apps/ui3/src/explorer/pages',
               'apps/ui3/src/landings', 'library/ui/components')
PROPERTY_DEFINITION = re.compile(r'(?<![\w-])(--[\w-]+)\s*:')
VAR_USE = re.compile(r'var\(\s*(--[\w-]+)')


def primitive_names(css=None):
    """The custom properties apps/ui3 atoms/primitives.css defines."""
    text = PRIMITIVES_CSS.read_text(encoding='utf-8') if css is None else css
    return set(PROPERTY_DEFINITION.findall(CSS_COMMENT.sub(' ', text)))


def floor_counts(text, primitives):
    """A stylesheet's var(--dcl- uses and its var() uses of a primitives property."""
    body = CSS_COMMENT.sub(' ', text)
    return {'tokens': len(TOKEN_USE.findall(body)),
            'primitives': sum(1 for name in VAR_USE.findall(body) if name in primitives)}


def current_floor(primitives=None):
    primitives = primitive_names() if primitives is None else primitives
    return {relative(file): floor_counts(file.read_text(encoding='utf-8', errors='replace'), primitives)
            for file in stylesheets(FLOOR_PATHS)}


def recorded_floor(current):
    """The floor to write: every stylesheet that uses a token or a primitives property."""
    return {name: counts for name, counts in current.items() if any(counts.values())}


def floor_issues(floor, current):
    """Each stylesheet whose token uses fell below, or whose primitives uses rose above, its floor,
    and each floor entry whose stylesheet is gone."""
    issues = [f'{name}: the floor records a stylesheet that is not there; run --write-token-floor'
              for name in sorted(floor.keys() - current.keys())]
    for name, counts in sorted(current.items()):
        recorded = floor.get(name, {'tokens': 0, 'primitives': 0})
        if counts['tokens'] < recorded['tokens']:
            issues.append(f"{name}: {counts['tokens']} var(--dcl- uses, the floor is {recorded['tokens']}")
        if counts['primitives'] > recorded['primitives']:
            issues.append(f"{name}: {counts['primitives']} uses of atoms/primitives.css properties, "
                          f"the ceiling is {recorded['primitives']}")
    return issues


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', help='Compare changes with this Git revision (default: merge base with origin/main)')
    parser.add_argument('--staged', action='store_true', help='Check the index instead of the working tree')
    parser.add_argument('--tokens', nargs='+', metavar='PATH',
                        help='Report token coverage for the stylesheets under these paths instead of checking')
    parser.add_argument('--token-floor', action='store_true',
                        help='Check the token floor of the stylesheets under FLOOR_PATHS instead')
    parser.add_argument('--write-token-floor', action='store_true',
                        help='Record the current token counts as the floor')
    args = parser.parse_args()
    if args.tokens:
        print('\n'.join(tokens_report(args.tokens)))
        return 0
    if args.write_token_floor:
        FLOOR_FILE.write_text(json.dumps(recorded_floor(current_floor()), indent=2, sort_keys=True) + '\n',
                              encoding='utf-8')
        return 0
    if args.token_floor:
        current = current_floor()
        issues = floor_issues(json.loads(FLOOR_FILE.read_text(encoding='utf-8')), current)
        if issues:
            print('\n'.join(issues), file=sys.stderr)
            return 1
        print(f'UI standards: token floor holds for {len(current)} stylesheets')
        return 0
    try:
        issues = check(args.base or default_base(), staged=args.staged)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f'UI standards check could not run: {error}', file=sys.stderr)
        return 2
    if issues:
        print('\n'.join(issues), file=sys.stderr)
        return 1
    print('UI standards: checked changed files for CSS-in-JS imports and new component stylesheets')
    return 0


if __name__ == '__main__':
    sys.exit(main())
