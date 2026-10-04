#pragma once

#include "rust/cxx.h"

namespace fcitx {
class AddonFactory;
}

namespace candidate_translator {
struct Translator;
struct TranslationResult;
fcitx::AddonFactory *addon_factory();
bool cpp_self_test();
bool cpp_candidate_validation_test();
rust::Vec<TranslationResult> cpp_wait_for_results(const Translator &translator);
} // namespace candidate_translator
