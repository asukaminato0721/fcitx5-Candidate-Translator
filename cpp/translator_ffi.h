#pragma once

#include <cstddef>
#include <cstdint>

extern "C" {

struct CtResult;
using CtResultCallback = void (*)(void *, std::uint64_t, CtResult *);

void ct_configure(bool enabled, const char *base_url, const char *model,
                  const char *api_key, std::uint64_t timeout_ms,
                  std::uint64_t debounce_ms, std::size_t cache_entries,
                  const char *cache_path);
char *ct_lookup(const char *target, const char *source);
void ct_submit(std::uint64_t request_id, const char *target,
               const std::uint32_t *indices, const char *const *sources,
               std::size_t len, void *user_data, CtResultCallback callback);
void ct_clear_cache();
void ct_string_free(char *value);
std::size_t ct_result_len(const CtResult *result);
std::uint32_t ct_result_index(const CtResult *result, std::size_t pos);
const char *ct_result_text(const CtResult *result, std::size_t pos);
const char *ct_result_error(const CtResult *result);
void ct_result_free(CtResult *result);
void ct_shutdown();

}
