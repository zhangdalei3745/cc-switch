import { render, screen } from "@testing-library/react";
import type { ComponentProps, PropsWithChildren } from "react";
import { useForm } from "react-hook-form";
import { describe, expect, it, vi } from "vitest";
import { CodexFormFields } from "@/components/providers/forms/CodexFormFields";
import { Form } from "@/components/ui/form";

type CodexFormFieldsProps = ComponentProps<typeof CodexFormFields>;

function FormShell({ children }: PropsWithChildren) {
  const form = useForm();
  return <Form {...form}>{children}</Form>;
}

function renderJoycodeForm(overrides: Partial<CodexFormFieldsProps> = {}) {
  const props: CodexFormFieldsProps = {
    isJoycodeProvider: true,
    codexApiKey: "BJ.joycode-key",
    onApiKeyChange: vi.fn(),
    category: "cn_official",
    shouldShowApiKeyLink: false,
    websiteUrl: "",
    shouldShowSpeedTest: false,
    codexBaseUrl: "https://example.invalid",
    onBaseUrlChange: vi.fn(),
    isFullUrl: false,
    onFullUrlChange: vi.fn(),
    isEndpointModalOpen: false,
    onEndpointModalToggle: vi.fn(),
    autoSelect: false,
    onAutoSelectChange: vi.fn(),
    codexModel: "GPT-5.6 Sol",
    onModelChange: vi.fn(),
    apiFormat: "openai_responses",
    onApiFormatChange: vi.fn(),
    anthropicAuthField: "ANTHROPIC_AUTH_TOKEN",
    onAnthropicAuthFieldChange: vi.fn(),
    impersonateClaudeCode: false,
    onImpersonateClaudeCodeChange: vi.fn(),
    maxOutputTokens: "",
    onMaxOutputTokensChange: vi.fn(),
    codexChatReasoning: {},
    onCodexChatReasoningChange: vi.fn(),
    promptCacheRouting: "auto",
    onPromptCacheRoutingChange: vi.fn(),
    catalogModels: [
      {
        model: "GPT-5.6 Sol",
        displayName: "GPT-5.6 Sol",
        contextWindow: 200000,
      },
      {
        model: "Claude-Opus-4.8-hq",
        displayName: "Claude Opus 4.8",
        contextWindow: 1000000,
      },
    ],
    onCatalogModelsChange: vi.fn(),
    speedTestEndpoints: [],
    customUserAgent: "",
    onCustomUserAgentChange: vi.fn(),
    localProxyHeadersOverride: "",
    onLocalProxyHeadersOverrideChange: vi.fn(),
    localProxyBodyOverride: "",
    onLocalProxyBodyOverrideChange: vi.fn(),
    ...overrides,
  };

  return render(
    <FormShell>
      <CodexFormFields {...props} />
    </FormShell>,
  );
}

describe("CodexFormFields", () => {
  it("JoyCode 显示默认模型与 /model 目录，并隐藏重复的协议和认证配置", () => {
    renderJoycodeForm();

    expect(screen.getByLabelText("默认模型")).toHaveValue("GPT-5.6 Sol");
    expect(
      screen.getByText(/这里的模型会出现在 Codex \/model 菜单中/),
    ).toBeInTheDocument();
    expect(screen.queryByLabelText("API Key")).not.toBeInTheDocument();
    expect(screen.queryByText("上游格式")).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "providerForm.fetchModels" }),
    ).not.toBeInTheDocument();

    expect(screen.getByRole("button", { name: "Select model" })).toBeVisible();
    expect(screen.getByDisplayValue("Claude-Opus-4.8-hq")).toBeInTheDocument();
  });
});
