/* C ABI over the inseam node core. Hand-maintained mirror of
 * crates/inseam-ffi/src/lib.rs — keep the two in sync.
 *
 * Conventions:
 * - Fallible calls take `char **error_out`; on failure they return NULL and,
 *   when error_out is non-NULL, store a message the caller must free.
 * - Every char* returned by this library is freed with inseam_string_free,
 *   and nodes with inseam_node_free.
 */

#ifndef INSEAM_FFI_H
#define INSEAM_FFI_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* An open node plus the runtime driving its async operations. Opaque. */
typedef struct InseamNode InseamNode;

/* The core library version as a fresh string. */
char *inseam_version(void);

/* Read effective first-party settings, with plugin defaults, as JSON. */
char *inseam_settings_read(const char *composition_path, char **error_out);

/* Validate and atomically write settings JSON. Custom entries are retained. */
bool inseam_settings_write(const char *composition_path,
                           const char *settings_json,
                           char **error_out);

/* Open the node under data_dir. composition_path may be NULL: then
 * <data_dir>/composition.toml is layered over the built-in base composition
 * when present. */
InseamNode *inseam_node_open(const char *data_dir,
                             const char *composition_path,
                             char **error_out);

/* Close a node and release its runtime. NULL is a no-op. */
void inseam_node_free(InseamNode *node);

/* Run a finder query. Returns the QueryResponse as JSON. */
char *inseam_node_query(const InseamNode *node,
                        const char *text,
                        uint32_t limit,
                        char **error_out);

/* Index a directory of the local filesystem host. Returns a JSON IndexReport. */
char *inseam_node_index_dir(const InseamNode *node,
                            const char *dir,
                            bool rebuild,
                            char **error_out);

/* The hosts this node stewards, as a JSON array of HostView. */
char *inseam_node_hosts(const InseamNode *node, char **error_out);

/* The OAuth grants this node holds, as a JSON array of GrantView
 * ({id, provider, scopes, client_id_env, client_secret_env, state}). */
char *inseam_node_grants(const InseamNode *node, char **error_out);

/* Begin authorizing a grant over the loopback redirect. Returns JSON
 * {grant, url, state, redirect_uri}: open url in the owner's browser, then
 * block on inseam_node_authorize_await with state. */
char *inseam_node_authorize_begin(const InseamNode *node,
                                  const char *grant,
                                  char **error_out);

/* Wait for a begun authorization to finish (blocks up to the oauth entry's
 * timeout — call off the main thread). Returns the grant's GrantView JSON. */
char *inseam_node_authorize_await(const InseamNode *node,
                                  const char *state,
                                  char **error_out);

/* Forget a grant's tokens. Returns the grant's GrantView JSON afterwards. */
char *inseam_node_revoke_grant(const InseamNode *node,
                               const char *grant,
                               char **error_out);

/* Every composition entry as the kernel runs it: a JSON array of PluginView
 * ({id, plugin, state: {state: "active"|"pending"|"failed", reason?},
 * effects, missing, missing_secrets}) — the `plugins` owner operation. */
char *inseam_node_plugins(const InseamNode *node, char **error_out);

/* Install a loaded plugin into the open node, no reopen. request_json is an
 * InstallPluginRequest: {id, files: [{path, bytes}], config?} — the plugin
 * directory's files, paths relative to it, bytes in standard base64 (at most
 * 64 files, 32 MiB total). Files land under <data_dir>/plugins/<id>/, the
 * entry is appended to the node's composition, the kernel reconciles in
 * place. Returns the new entry's PluginView JSON; an entry that fails to
 * activate is rolled back and the failure is the error. Blocks for the
 * mount (admission runs on first sighting) — call off the main thread. */
char *inseam_node_install_plugin(const InseamNode *node,
                                 const char *request_json,
                                 char **error_out);

/* Per-entry health as a JSON array of {id, plugin, state, error, missing}.
 * state is "active" | "pending" | "failed"; error is set only for failed
 * entries and missing only for pending ones. */
char *inseam_node_health(const InseamNode *node, char **error_out);

/* Free a string returned by this library. NULL is a no-op. */
void inseam_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* INSEAM_FFI_H */
