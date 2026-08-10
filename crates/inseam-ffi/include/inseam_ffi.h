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

/* Open the node under data_dir. profile_path may be NULL: then
 * <data_dir>/profile.toml is used when present, else the default profile. */
InseamNode *inseam_node_open(const char *data_dir,
                             const char *profile_path,
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

/* Free a string returned by this library. NULL is a no-op. */
void inseam_string_free(char *s);

#ifdef __cplusplus
}
#endif

#endif /* INSEAM_FFI_H */
