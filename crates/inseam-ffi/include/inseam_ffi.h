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

/* Validate and write first-party settings without changing the embedder
 * entry. Used by an app shell that mounts its own on-device provider. */
bool inseam_settings_write_preserving_embedder(const char *composition_path,
                                               const char *settings_json,
                                               char **error_out);

/* Open the node under data_dir. composition_path may be NULL: then
 * <data_dir>/composition.toml is layered over the built-in base composition
 * when present. */
InseamNode *inseam_node_open(const char *data_dir,
                             const char *composition_path,
                             char **error_out);

/* The shell's own embedder (an on-device model), so a node needs no API
 * key for vectors. embed receives count texts as a JSON array of strings
 * and returns count × dimensions float32s in one row-major buffer, or
 * NULL with error_out set. Callbacks may run on any thread, several at
 * once. Strings are freed through free_string, buffers through
 * free_floats; release runs exactly once at inseam_node_free. */
typedef struct InseamEmbedderCallbacks {
    float *(*embed)(void *user_data,
                    const char *texts_json,
                    uint32_t count,
                    char **error_out);
    void (*free_string)(void *user_data, char *s);
    void (*free_floats)(void *user_data, float *floats, uint64_t count);
    void (*release)(void *user_data);
} InseamEmbedderCallbacks;

/* Open the node with the shell's embedder mounted: embedder_json is
 * {model, dimensions} (dimensions 1..=4096); callbacks is copied. The base
 * composition's `embedder` entry is re-pointed at the `embedder-app` plugin
 * beneath the node's own composition.toml, which still wins. On failure
 * release is NOT called. */
InseamNode *inseam_node_open_with_shell(const char *data_dir,
                                        const char *composition_path,
                                        const char *embedder_json,
                                        const InseamEmbedderCallbacks *callbacks,
                                        void *user_data,
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

/* Live control for one blocking index call. update receives an
 * IndexProgress JSON snapshot and returns 0 to continue, 1 to pause, or 2
 * to stop. A paused run polls update four times per second. The callback and
 * user_data are borrowed until the call returns and may be used from any
 * thread. */
typedef struct InseamIndexCallbacks {
    uint32_t (*update)(void *user_data, const char *progress_json);
} InseamIndexCallbacks;

char *inseam_node_index_dir_controlled(const InseamNode *node,
                                       const char *dir,
                                       bool rebuild,
                                       const InseamIndexCallbacks *callbacks,
                                       void *user_data,
                                       char **error_out);

/* An app-bridged host: the shell enumerates and reads a host only it can
 * reach (a Photos library, a Notes folder) and the node catalogs, indexes,
 * and serves it like any other. Every callback may run on any thread, and
 * several at once. Strings the shell returns (results and error_out
 * messages) are freed through free_string, buffers through free_bytes;
 * release runs exactly once when the node is done with the host — after an
 * unregister and after every in-flight read — and is where the shell frees
 * user_data. */
typedef struct InseamHostCallbacks {
    /* JSON array of {locator, source_type, content_type, bytes, created?,
     * modified?, title?, properties?: [{key, value}]} under root
     * (timestamps are Unix seconds). NULL with error_out set on failure. */
    char *(*enumerate)(void *user_data, const char *root, char **error_out);
    /* The raw bytes of one locator; length in *len_out. NULL with error_out
     * set on failure. At most 256 MiB. */
    uint8_t *(*read_bytes)(void *user_data,
                           const char *locator,
                           uint64_t *len_out,
                           char **error_out);
    void (*free_string)(void *user_data, char *s);
    void (*free_bytes)(void *user_data, uint8_t *bytes, uint64_t len);
    void (*release)(void *user_data);
} InseamHostCallbacks;

/* Register a bridged host. host_json is {kind, principal, display_name,
 * capabilities?: {enumerates, change_feed, writable}}; the host id is
 * derived from kind and principal. callbacks is copied. Returns the
 * HostView JSON, or NULL with the reason — and then release is NOT called.
 * At most 32 bridged hosts per node. */
char *inseam_node_register_host(const InseamNode *node,
                                const char *host_json,
                                const InseamHostCallbacks *callbacks,
                                void *user_data,
                                char **error_out);

/* Withdraw a bridged host. false with error_out set when no bridged host
 * has that id. release may run after this returns. */
bool inseam_node_unregister_host(const InseamNode *node,
                                 const char *host_id,
                                 char **error_out);

/* Index a scope (root; "" for all) of one stewarded host. Returns a JSON
 * IndexReport. */
char *inseam_node_index_host(const InseamNode *node,
                             const char *host_id,
                             const char *root,
                             bool rebuild,
                             char **error_out);

char *inseam_node_index_host_controlled(const InseamNode *node,
                                        const char *host_id,
                                        const char *root,
                                        bool rebuild,
                                        const InseamIndexCallbacks *callbacks,
                                        void *user_data,
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
