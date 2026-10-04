#include "fcitx5-candidate-translator/src/bridge.rs.h"

#include <algorithm>
#include <cstdint>
#include <filesystem>
#include <memory>
#include <string>
#include <stdexcept>
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
#include <fcitx-utils/event.h>
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

namespace backend = candidate_translator;

constexpr char kConfigPath[] = "conf/candidate-translator.conf";

enum class TargetLanguage { English, Japanese };
FCITX_CONFIG_ENUM_NAME_WITH_I18N(TargetLanguage, N_("English"), N_("Japanese"))

enum class ReasoningEffortMode { Auto, None, Low, Medium };
FCITX_CONFIG_ENUM_NAME_WITH_I18N(ReasoningEffortMode, N_("Auto"), N_("None"),
                                 N_("Low"), N_("Medium"))

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
    fcitx::Option<bool> showKanaReading{
        this, "ShowKanaReading", _("Show kana reading for Japanese translations"),
        true};
    fcitx::OptionWithAnnotation<ReasoningEffortMode,
                                ReasoningEffortModeI18NAnnotation>
        reasoningEffort{this, "ReasoningEffort", _("Reasoning effort"),
                        ReasoningEffortMode::Auto};
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

struct ContextState {
    std::shared_ptr<fcitx::CandidateList> list;
    std::unordered_map<std::string, std::string> translations;
    std::string signature;
    std::uint64_t currentRequest = 0;
};

struct PendingRequest {
    fcitx::InputContext *inputContext;
    std::shared_ptr<fcitx::CandidateList> list;
    std::string signature;
};

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

class CandidateTranslatorAddon final : public fcitx::AddonInstance {
public:
    explicit CandidateTranslatorAddon(fcitx::Instance *instance)
        : instance_(instance), translator_(backend::new_translator()) {
        resultEvent_ = instance_->eventLoop().addIOEvent(
            translator_->result_fd(), fcitx::IOEventFlag::In,
            [this](fcitx::EventSourceIO *, int, fcitx::IOEventFlags) {
                for (auto &result : translator_->take_results()) {
                    applyResult(std::move(result));
                }
                return true;
            });
        if (!resultEvent_) {
            throw std::runtime_error("Failed to watch translator results");
        }
        reloadConfig();
        outputConnection_ = instance_->connect<fcitx::Instance::OutputFilter>(
            [this](fcitx::InputContext *inputContext, fcitx::Text &text) {
                filterOutput(inputContext, text);
            });
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
        // Unregister the borrowed fd before Translator joins its worker and
        // closes the sockets. Background work never accesses this addon.
        resultEvent_.reset();
        clearAll();
    }

    const fcitx::Configuration *getConfig() const override { return &config_; }

    void setConfig(const fcitx::RawConfig &rawConfig) override {
        clearAll();
        config_.load(rawConfig, true);
        if (*config_.clearCache) {
            translator_->clear_cache();
            config_.clearCache.setValue(false);
        }
        fcitx::safeSaveAsIni(config_, kConfigPath);
        configureBackend();
    }

    void reloadConfig() override {
        clearAll();
        fcitx::readAsIni(config_, kConfigPath);
        configureBackend();
    }

private:
    std::string targetLanguage() const {
        if (*config_.targetLanguage != TargetLanguage::Japanese) {
            return "English";
        }
        return *config_.showKanaReading ? "JapaneseWithKana" : "Japanese";
    }

    bool configured() const {
        return *config_.enabled && !config_.baseUrl->empty() &&
               !config_.model->empty() && !config_.apiKey->empty();
    }

    std::string reasoningEffort() const {
        switch (*config_.reasoningEffort) {
        case ReasoningEffortMode::None:
            return "none";
        case ReasoningEffortMode::Low:
            return "low";
        case ReasoningEffortMode::Medium:
            return "medium";
        case ReasoningEffortMode::Auto:
            return "";
        }
        return "";
    }

    void configureBackend() {
        const auto cachePath =
            fcitx::StandardPaths::global()
                .userDirectory(fcitx::StandardPathsType::Cache) /
            "candidate-translator/cache-v1.json";
        const auto reasoning = reasoningEffort();
        const auto dictionaryPath = fcitx::StandardPaths::global().locate(
            fcitx::StandardPathsType::PkgData,
            "candidate-translator/cedict_ts.csv");
        translator_->configure(backend::BackendConfig{
            .enabled = *config_.enabled,
            .base_url = *config_.baseUrl,
            .model = *config_.model,
            .api_key = *config_.apiKey,
            .reasoning_effort = reasoning,
            .dictionary_path = dictionaryPath.string(),
            .timeout_ms = static_cast<std::uint64_t>(*config_.requestTimeoutMs),
            .debounce_ms = static_cast<std::uint64_t>(*config_.debounceMs),
            .cache_entries = static_cast<std::size_t>(*config_.cacheEntries),
            .cache_path = cachePath.string(),
        });
        const auto configPath =
            fcitx::StandardPaths::global()
                .userDirectory(fcitx::StandardPathsType::Config) /
            kConfigPath;
        ::chmod(configPath.c_str(), 0600);
    }

    void clearAll() {
        translator_->cancel_requests();
        contexts_.clear();
        pending_.clear();
    }

    void removeInputContext(fcitx::InputContext *inputContext) {
        contexts_.erase(inputContext);
        std::erase_if(pending_, [inputContext](const auto &entry) {
            return entry.second.inputContext == inputContext;
        });
    }

    void filterOutput(fcitx::InputContext *inputContext, fcitx::Text &text) {
        auto state = contexts_.find(inputContext);
        if (state == contexts_.end()) {
            return;
        }
        auto currentList = inputContext->inputPanel().candidateList();
        if (!currentList || currentList.get() != state->second.list.get()) {
            return;
        }
        const auto source = text.toStringForCommit();
        auto translation = state->second.translations.find(source);
        if (translation == state->second.translations.end()) {
            return;
        }
        text.append("  ");
        text.append(translation->second, fcitx::TextFormatFlag::Italic);
    }

    void updateInputContext(fcitx::InputContext *inputContext) {
        auto list = inputContext->inputPanel().candidateList();
        auto &state = contexts_[inputContext];
        const bool sensitive = inputContext->capabilityFlags().testAny(
            fcitx::CapabilityFlag::PasswordOrSensitive);
        if (!configured() || sensitive || !list || list->empty()) {
            if (state.currentRequest != 0) {
                pending_.erase(state.currentRequest);
            }
            state = {};
            return;
        }

        if (state.list.get() != list.get()) {
            if (state.currentRequest != 0) {
                pending_.erase(state.currentRequest);
            }
            state = {};
            state.list = list;
        }

        std::string signature = targetLanguage();
        state.translations.clear();
        rust::Vec<backend::Candidate> missing;
        for (int index = 0; index < list->size(); ++index) {
            const auto &word = list->candidate(index);
            const auto source = word.text().toStringForCommit();
            signature.append("\x1f").append(source);
            if (!containsHan(source) || utf8Characters(source) > 32 ||
                word.isPlaceHolder()) {
                continue;
            }
            auto cached = translator_->lookup(targetLanguage(), source);
            if (!cached.empty()) {
                state.translations.insert_or_assign(source, std::string(cached));
            } else if (missing.size() < 64) {
                missing.push_back(backend::Candidate{
                    .index = static_cast<std::uint32_t>(index), .source = source});
            }
        }
        if (state.signature != signature && state.currentRequest != 0) {
            pending_.erase(state.currentRequest);
            state.currentRequest = 0;
        }
        state.signature = signature;
        if (missing.empty() || state.currentRequest != 0) {
            return;
        }

        const auto requestId = nextRequestId_++;
        state.currentRequest = requestId;
        pending_.emplace(requestId,
                         PendingRequest{inputContext, list, signature});
        translator_->submit(requestId, targetLanguage(), std::move(missing));
    }

    void applyResult(backend::TranslationResult result) {
        auto pending = pending_.find(result.request_id);
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
                FCITX_WARN() << "Candidate translation failed: " << std::string(result.error);
            }
            return;
        }
        for (const auto &[index, translation] : result.translations) {
            if (index >= static_cast<std::uint32_t>(currentList->size()) ||
                translation.empty()) {
                continue;
            }
            const auto &word =
                currentList->candidate(static_cast<int>(index));
            state.translations.insert_or_assign(
                word.text().toStringForCommit(), std::string(translation));
        }
        request.inputContext->updateUserInterface(
            fcitx::UserInterfaceComponent::InputPanel);
    }

    fcitx::Instance *instance_;
    TranslatorConfig config_;
    fcitx::Connection outputConnection_;
    std::vector<std::unique_ptr<fcitx::HandlerTableEntry<fcitx::EventHandler>>>
        handlers_;
    std::unordered_map<fcitx::InputContext *, ContextState> contexts_;
    std::unordered_map<std::uint64_t, PendingRequest> pending_;
    std::uint64_t nextRequestId_ = 1;
    rust::Box<backend::Translator> translator_;
    // Destruction order matters: the watcher must die before its borrowed fd.
    std::unique_ptr<fcitx::EventSourceIO> resultEvent_;
};

class CandidateTranslatorFactory final : public fcitx::AddonFactory {
public:
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override {
        return new CandidateTranslatorAddon(manager->instance());
    }
};

} // namespace

bool candidate_translator::cpp_self_test() {
    fcitx::Text display("candidate");
    display.append("  ");
    display.append("translation", fcitx::TextFormatFlag::Italic);
    return display.toString() == "candidate  translation";
}

rust::Vec<candidate_translator::TranslationResult>
candidate_translator::cpp_wait_for_results(const Translator &translator) {
    fcitx::EventLoop loop;
    rust::Vec<TranslationResult> results;
    auto watcher = loop.addIOEvent(
        translator.result_fd(), fcitx::IOEventFlag::In,
        [&](fcitx::EventSourceIO *, int, fcitx::IOEventFlags) {
            results = translator.take_results();
            loop.exit();
            return true;
        });
    auto timeout = loop.addTimeEvent(
        CLOCK_MONOTONIC, fcitx::now(CLOCK_MONOTONIC) + 5'000'000, 0,
        [&](fcitx::EventSourceTime *, std::uint64_t) {
            loop.exit();
            return false;
        });
    if (!watcher || !timeout) {
        return results;
    }
    loop.exec();
    return results;
}

fcitx::AddonFactory *candidate_translator::addon_factory() {
    static CandidateTranslatorFactory factory;
    return &factory;
}
