#include "translator_ffi.h"

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <string>
#include <unordered_map>
#include <utility>
#include <vector>

#include <sys/stat.h>

#include <fcitx-config/configuration.h>
#include <fcitx-config/iniparser.h>
#include <fcitx-utils/capabilityflags.h>
#include <fcitx-utils/i18n.h>
#include <fcitx-utils/log.h>
#include <fcitx-utils/standardpaths.h>
#include <fcitx-utils/trackableobject.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/candidatelist.h>
#include <fcitx/event.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputpanel.h>
#include <fcitx/instance.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>

namespace {

constexpr char kConfigPath[] = "conf/candidate-translator.conf";

enum class TargetLanguage { English, Japanese };
FCITX_CONFIG_ENUM_NAME_WITH_I18N(TargetLanguage, N_("English"), N_("Japanese"))

FCITX_CONFIGURATION(
    TranslatorConfig,
    fcitx::Option<bool> enabled{this, "Enabled", _("Enable candidate translation"), true};
    fcitx::Option<std::string> baseUrl{
        this, "BaseURL", _("OpenAI-compatible Base URL"), ""};
    fcitx::Option<std::string> model{this, "Model", _("Model"), ""};
    fcitx::Option<std::string> apiKey{this, "APIKey", _("API Key (stored as plain text)"), ""};
    fcitx::OptionWithAnnotation<TargetLanguage, TargetLanguageI18NAnnotation>
        targetLanguage{this, "TargetLanguage", _("Target language"),
                       TargetLanguage::English};
    fcitx::Option<int, fcitx::IntConstrain> debounceMs{
        this, "DebounceMs", _("Request debounce (milliseconds)"), 180,
        fcitx::IntConstrain(0, 2000)};
    fcitx::Option<int, fcitx::IntConstrain> requestTimeoutMs{
        this, "RequestTimeoutMs", _("Request timeout (milliseconds)"), 3000,
        fcitx::IntConstrain(500, 15000)};
    fcitx::Option<int, fcitx::IntConstrain> cacheEntries{
        this, "CacheEntries", _("Maximum cached translations"), 2048,
        fcitx::IntConstrain(0, 100000)};
    fcitx::Option<bool> clearCache{
        this, "ClearCache", _("Clear translation cache on Apply"), false};)

class CandidateCommentAccess : public fcitx::CandidateWord {
public:
    static void set(fcitx::CandidateWord &word, fcitx::Text text) {
        auto setter = &CandidateCommentAccess::setComment;
        (word.*setter)(std::move(text));
    }

    void select(fcitx::InputContext *) const override {}
};

struct Decoration {
    fcitx::CandidateWord *word;
    fcitx::Text original;
    std::string applied;
    std::string translation;
};

struct ContextState {
    std::shared_ptr<fcitx::CandidateList> list;
    std::vector<Decoration> decorations;
    std::string signature;
    std::uint64_t currentRequest = 0;
};

struct PendingRequest {
    fcitx::InputContext *inputContext;
    std::shared_ptr<fcitx::CandidateList> list;
    std::string signature;
};

struct CallbackResult {
    std::uint64_t requestId;
    std::vector<std::pair<std::uint32_t, std::string>> translations;
    std::string error;
};

class CandidateTranslatorAddon;
struct CallbackContext {
    std::atomic<CandidateTranslatorAddon *> addon{nullptr};
};

bool containsCandidate(const std::shared_ptr<fcitx::CandidateList> &list,
                       const fcitx::CandidateWord *word) {
    if (!list) {
        return false;
    }
    if (auto *bulk = list->toBulk()) {
        for (int index = 0; index < bulk->totalSize(); ++index) {
            if (&bulk->candidateFromAll(index) == word) {
                return true;
            }
        }
        return false;
    }
    for (int index = 0; index < list->size(); ++index) {
        if (&list->candidate(index) == word) {
            return true;
        }
    }
    return false;
}

bool containsHan(std::string_view text) {
    for (std::size_t offset = 0; offset < text.size();) {
        const auto first = static_cast<unsigned char>(text[offset]);
        std::uint32_t codepoint = 0;
        std::size_t length = 1;
        if (first < 0x80) {
            codepoint = first;
        } else if ((first & 0xe0) == 0xc0 && offset + 1 < text.size()) {
            codepoint = ((first & 0x1f) << 6) |
                        (static_cast<unsigned char>(text[offset + 1]) & 0x3f);
            length = 2;
        } else if ((first & 0xf0) == 0xe0 && offset + 2 < text.size()) {
            codepoint = ((first & 0x0f) << 12) |
                        ((static_cast<unsigned char>(text[offset + 1]) & 0x3f)
                         << 6) |
                        (static_cast<unsigned char>(text[offset + 2]) & 0x3f);
            length = 3;
        } else if ((first & 0xf8) == 0xf0 && offset + 3 < text.size()) {
            codepoint = ((first & 0x07) << 18) |
                        ((static_cast<unsigned char>(text[offset + 1]) & 0x3f)
                         << 12) |
                        ((static_cast<unsigned char>(text[offset + 2]) & 0x3f)
                         << 6) |
                        (static_cast<unsigned char>(text[offset + 3]) & 0x3f);
            length = 4;
        }
        if ((codepoint >= 0x3400 && codepoint <= 0x9fff) ||
            (codepoint >= 0x20000 && codepoint <= 0x323af)) {
            return true;
        }
        offset += length;
    }
    return false;
}

std::size_t utf8Characters(std::string_view text) {
    return std::count_if(text.begin(), text.end(), [](char value) {
        return (static_cast<unsigned char>(value) & 0xc0) != 0x80;
    });
}

class CandidateTranslatorAddon final
    : public fcitx::AddonInstance,
      public fcitx::TrackableObject<CandidateTranslatorAddon> {
public:
    explicit CandidateTranslatorAddon(fcitx::Instance *instance)
        : instance_(instance), callbackContext_(std::make_unique<CallbackContext>()) {
        callbackContext_->addon.store(this);
        reloadConfig();
        handlers_.emplace_back(instance_->watchEvent(
            fcitx::EventType::InputContextUpdateUI,
            fcitx::EventWatcherPhase::Default,
            [this](fcitx::Event &event) {
                auto &update = static_cast<fcitx::InputContextUpdateUIEvent &>(event);
                if (update.component() == fcitx::UserInterfaceComponent::InputPanel) {
                    updateInputContext(update.inputContext());
                }
            }));
        handlers_.emplace_back(instance_->watchEvent(
            fcitx::EventType::InputContextDestroyed,
            fcitx::EventWatcherPhase::Default,
            [this](fcitx::Event &event) {
                auto &destroyed = static_cast<fcitx::InputContextDestroyedEvent &>(event);
                removeInputContext(destroyed.inputContext());
            }));
    }

    ~CandidateTranslatorAddon() override {
        callbackContext_->addon.store(nullptr);
        restoreAll();
        ct_shutdown();
    }

    const fcitx::Configuration *getConfig() const override { return &config_; }

    void setConfig(const fcitx::RawConfig &rawConfig) override {
        restoreAll();
        config_.load(rawConfig, true);
        if (*config_.clearCache) {
            ct_clear_cache();
            config_.clearCache.setValue(false);
        }
        fcitx::safeSaveAsIni(config_, kConfigPath);
        configureBackend();
    }

    void reloadConfig() override {
        restoreAll();
        fcitx::readAsIni(config_, kConfigPath);
        configureBackend();
    }

    void receiveResult(CallbackResult result) {
        auto reference = watch();
        instance_->eventDispatcher().scheduleWithContext(
            reference, [this, result = std::move(result)]() mutable {
                applyResult(std::move(result));
            });
    }

private:
    static void rustCallback(void *userData, std::uint64_t requestId,
                             CtResult *result) {
        CallbackResult copied{.requestId = requestId,
                              .translations = {},
                              .error = {}};
        const auto length = ct_result_len(result);
        copied.translations.reserve(length);
        for (std::size_t index = 0; index < length; ++index) {
            const auto *text = ct_result_text(result, index);
            copied.translations.emplace_back(
                ct_result_index(result, index), text ? text : "");
        }
        if (const auto *error = ct_result_error(result); error) {
            copied.error = error;
        }
        ct_result_free(result);
        auto *context = static_cast<CallbackContext *>(userData);
        if (auto *addon = context->addon.load()) {
            addon->receiveResult(std::move(copied));
        }
    }

    std::string targetLanguage() const {
        return *config_.targetLanguage == TargetLanguage::Japanese ? "Japanese"
                                                                   : "English";
    }

    bool configured() const {
        return *config_.enabled && !config_.baseUrl->empty() &&
               !config_.model->empty() && !config_.apiKey->empty();
    }

    void configureBackend() {
        const auto cachePath =
            fcitx::StandardPaths::global()
                .userDirectory(fcitx::StandardPathsType::Cache) /
            "candidate-translator/cache-v1.json";
        ct_configure(*config_.enabled, config_.baseUrl->c_str(),
                     config_.model->c_str(), config_.apiKey->c_str(),
                     *config_.requestTimeoutMs, *config_.debounceMs,
                     *config_.cacheEntries, cachePath.c_str());
        const auto configPath =
            fcitx::StandardPaths::global()
                .userDirectory(fcitx::StandardPathsType::Config) /
            kConfigPath;
        ::chmod(configPath.c_str(), 0600);
    }

    void restore(ContextState &state) {
        for (auto &decoration : state.decorations) {
            if (containsCandidate(state.list, decoration.word) &&
                decoration.word->comment().toString() == decoration.applied) {
                CandidateCommentAccess::set(*decoration.word,
                                            std::move(decoration.original));
            }
        }
        state.decorations.clear();
    }

    void restoreAll() {
        for (auto &[_, state] : contexts_) {
            restore(state);
        }
        contexts_.clear();
        pending_.clear();
    }

    void removeInputContext(fcitx::InputContext *inputContext) {
        if (auto iter = contexts_.find(inputContext); iter != contexts_.end()) {
            restore(iter->second);
            contexts_.erase(iter);
        }
        std::erase_if(pending_, [inputContext](const auto &entry) {
            return entry.second.inputContext == inputContext;
        });
    }

    void applyTranslation(ContextState &state, fcitx::CandidateWord &word,
                          const std::string &translation) {
        auto existing = std::find_if(
            state.decorations.begin(), state.decorations.end(),
            [&word](const Decoration &item) { return item.word == &word; });
        if (existing != state.decorations.end()) {
            if (existing->translation == translation &&
                word.comment().toString() == existing->applied) {
                return;
            }
            if (word.comment().toString() == existing->applied) {
                CandidateCommentAccess::set(word, existing->original);
            } else {
                existing->original = word.comment();
            }
            state.decorations.erase(existing);
        }

        fcitx::Text original = word.comment();
        fcitx::Text combined = original;
        if (!combined.empty()) {
            combined.append(" · ");
        }
        combined.append(translation);
        const auto applied = combined.toString();
        CandidateCommentAccess::set(word, std::move(combined));
        state.decorations.push_back(
            Decoration{&word, std::move(original), applied, translation});
    }

    void updateInputContext(fcitx::InputContext *inputContext) {
        auto list = inputContext->inputPanel().candidateList();
        auto &state = contexts_[inputContext];
        const bool sensitive = inputContext->capabilityFlags().testAny(
            fcitx::CapabilityFlag::PasswordOrSensitive);
        if (!configured() || sensitive ||
            instance_->inputMethod(inputContext) != "shuangpin" || !list ||
            list->empty()) {
            if (state.currentRequest != 0) {
                pending_.erase(state.currentRequest);
            }
            restore(state);
            state = {};
            return;
        }

        if (state.list.get() != list.get()) {
            if (state.currentRequest != 0) {
                pending_.erase(state.currentRequest);
            }
            restore(state);
            state = {};
            state.list = list;
        }

        std::string signature = targetLanguage();
        std::vector<std::uint32_t> missingIndices;
        std::vector<std::string> missingSources;
        for (int index = 0; index < list->size(); ++index) {
            auto &word = const_cast<fcitx::CandidateWord &>(list->candidate(index));
            const auto source = word.text().toStringForCommit();
            signature.append("\x1f").append(source);
            if (!containsHan(source) || utf8Characters(source) > 32 ||
                word.isPlaceHolder()) {
                continue;
            }
            char *cached = ct_lookup(targetLanguage().c_str(), source.c_str());
            if (cached) {
                applyTranslation(state, word, cached);
                ct_string_free(cached);
            } else {
                missingIndices.push_back(static_cast<std::uint32_t>(index));
                missingSources.push_back(source);
            }
        }
        if (state.signature != signature && state.currentRequest != 0) {
            pending_.erase(state.currentRequest);
            state.currentRequest = 0;
        }
        state.signature = signature;
        if (missingIndices.empty() || state.currentRequest != 0) {
            return;
        }

        const auto requestId = nextRequestId_++;
        state.currentRequest = requestId;
        pending_.emplace(requestId,
                         PendingRequest{inputContext, list, signature});
        std::vector<const char *> sourcePointers;
        sourcePointers.reserve(missingSources.size());
        for (const auto &source : missingSources) {
            sourcePointers.push_back(source.c_str());
        }
        const auto target = targetLanguage();
        ct_submit(requestId, target.c_str(), missingIndices.data(),
                  sourcePointers.data(), sourcePointers.size(),
                  callbackContext_.get(), &CandidateTranslatorAddon::rustCallback);
    }

    void applyResult(CallbackResult result) {
        auto pending = pending_.find(result.requestId);
        if (pending == pending_.end()) {
            return;
        }
        const auto request = std::move(pending->second);
        pending_.erase(pending);
        auto stateIter = contexts_.find(request.inputContext);
        if (stateIter == contexts_.end()) {
            return;
        }
        auto &state = stateIter->second;
        state.currentRequest = 0;
        auto currentList = request.inputContext->inputPanel().candidateList();
        if (state.signature != request.signature ||
            currentList.get() != request.list.get()) {
            return;
        }
        if (!result.error.empty()) {
            if (result.error != "translation request was superseded") {
                FCITX_WARN() << "Candidate translation failed: " << result.error;
            }
            return;
        }
        for (const auto &[index, translation] : result.translations) {
            if (index >= static_cast<std::uint32_t>(currentList->size()) ||
                translation.empty()) {
                continue;
            }
            auto &word = const_cast<fcitx::CandidateWord &>(
                currentList->candidate(static_cast<int>(index)));
            applyTranslation(state, word, translation);
        }
        request.inputContext->updateUserInterface(
            fcitx::UserInterfaceComponent::InputPanel);
    }

    fcitx::Instance *instance_;
    TranslatorConfig config_;
    std::vector<std::unique_ptr<fcitx::HandlerTableEntry<fcitx::EventHandler>>>
        handlers_;
    std::unordered_map<fcitx::InputContext *, ContextState> contexts_;
    std::unordered_map<std::uint64_t, PendingRequest> pending_;
    std::uint64_t nextRequestId_ = 1;
    std::unique_ptr<CallbackContext> callbackContext_;
};

class CandidateTranslatorFactory final : public fcitx::AddonFactory {
public:
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override {
        return new CandidateTranslatorAddon(manager->instance());
    }
};

} // namespace

extern "C" bool ct_cpp_self_test() {
    class TestCandidate final : public fcitx::CandidateWord {
    public:
        TestCandidate() : fcitx::CandidateWord(fcitx::Text("candidate")) {
            setComment(fcitx::Text("original"));
        }
        void select(fcitx::InputContext *) const override {}
    } candidate;

    fcitx::CandidateWord &base = candidate;
    CandidateCommentAccess::set(base, fcitx::Text("translated"));
    const bool typePreserved = dynamic_cast<TestCandidate *>(&base) != nullptr;
    const bool commentApplied = base.comment().toString() == "translated";
    CandidateCommentAccess::set(base, fcitx::Text("original"));
    return typePreserved && commentApplied &&
           base.comment().toString() == "original";
}

extern "C" fcitx::AddonFactory *ct_cpp_addon_factory_instance() {
    static CandidateTranslatorFactory factory;
    return &factory;
}
