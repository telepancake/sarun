/*
 * sud/cmd-rewrite/cmd_rewrite.h — Public surface of the cmd-rewrite addin.
 *
 * cmd-rewrite intercepts SYS_execve / SYS_execveat and rewrites the
 * (path, argv) pair according to user-configured rules:
 *
 *   compiler-wrap  prepends a tool (typically ccache) to argv when
 *                  the binary about to be exec'd matches.  Generic
 *                  pattern matchers (basename / fnmatch glob /
 *                  exact path) cover the vendor-cross-compiler case
 *                  ("arm-foo-bar-gcc-12.2") without hardcoded names.
 *
 *   exec-strip     drops a wrapper command (sudo, fakeroot-ng, env)
 *                  from argv and runs the inner program directly,
 *                  keeping its env intact.  Built-in flag-skip
 *                  tables for the well-known wrappers; custom
 *                  flag-skip spec available per rule.
 *
 *   exec-as        bumps the runtime config's pretend-uid (and
 *                  optional gid) for the rewritten exec and every
 *                  descendant, so the existing fakeroot uid-getter
 *                  short-circuit reports that uid.  Lets you say
 *                  "every `make install` invocation thinks it's
 *                  root, but only inside this build subtree."
 *
 * Generic suppression mechanism: each rule has an implicit name
 * "<kind>:<match>:<pattern>".  Once a rule fires, its name is
 * appended to the runtime config's suppressed[] list so the wrapper-
 * rewrite path re-emits it onto every child wrapper's argv as
 * --suppress-rule.  Children never re-fire that rule, killing the
 * recursive ccache→ccache→ccache loop without per-program plumbing.
 *
 * The addin is slotted between path_remap and fake-exec in the
 * dispatch chain (see sud/addin.c): paths are already resolved by
 * the time we see them, and any further elision (true/false/echo)
 * gets the rewritten argv as input.  So `sudo /usr/bin/true`
 * becomes /usr/bin/true via exec-strip, then fake-exec elides it.
 */

#ifndef SUD_CMD_REWRITE_H
#define SUD_CMD_REWRITE_H

#include "sud/addin.h"

extern const struct sud_addin sud_cmd_rewrite_addin;

#endif /* SUD_CMD_REWRITE_H */
