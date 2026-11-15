/**
 * The environment the reference build, the sites build and the sites preview get: what tools need to
 * run, nothing that points at a real service or switches an app's mode, so a capture does not depend
 * on who runs it.
 */
export function childEnv(): NodeJS.ProcessEnv {
  return Object.fromEntries(Object.entries(process.env).filter(([k]) => /^(PATH|HOME|USER|TMPDIR|TMP|TEMP|LANG|LC_ALL|SHELL|PNPM_HOME|COREPACK_HOME|npm_config_cache|NODE_EXTRA_CA_CERTS|SSL_CERT_FILE)$/.test(k)));
}

/** The value after a command-line flag, when the flag is given. */
export function arg(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}
