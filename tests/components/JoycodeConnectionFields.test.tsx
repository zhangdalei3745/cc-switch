import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { JoycodeConnectionFields } from "@/components/providers/forms/JoycodeConnectionFields";

const modelFetchMocks = vi.hoisted(() => ({
  fetchJoycodeModels: vi.fn(),
  importJoycodeCredential: vi.fn(),
  validateJoycodeCredential: vi.fn(),
}));
const toastMocks = vi.hoisted(() => ({
  error: vi.fn(),
  info: vi.fn(),
  success: vi.fn(),
}));

vi.mock("@/lib/api/model-fetch", () => ({
  fetchJoycodeModels: modelFetchMocks.fetchJoycodeModels,
  importJoycodeCredential: modelFetchMocks.importJoycodeCredential,
  validateJoycodeCredential: modelFetchMocks.validateJoycodeCredential,
}));

vi.mock("sonner", () => ({ toast: toastMocks }));

function ControlledFields({
  onCredential = vi.fn(),
}: {
  onCredential?: (credential: string) => void;
}) {
  const [credential, setCredential] = useState("");
  return (
    <JoycodeConnectionFields
      network="internal"
      onNetworkChange={vi.fn()}
      credential={credential}
      onCredential={(value) => {
        setCredential(value);
        onCredential(value);
      }}
    />
  );
}

describe("JoycodeConnectionFields", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("uses a manual password field instead of opening a web login", () => {
    const onCredential = vi.fn();
    render(<ControlledFields onCredential={onCredential} />);

    expect(
      screen.queryByRole("button", { name: "打开 JoyCode 登录并自动导入" }),
    ).not.toBeInTheDocument();

    const input = screen.getByLabelText("JoyCode ptKey");
    expect(input).toHaveAttribute("type", "password");
    expect(screen.getByRole("button", { name: "获取模型" })).toBeDisabled();

    fireEvent.change(input, { target: { value: "  BJ.manual-key  " } });

    expect(onCredential).toHaveBeenLastCalledWith("BJ.manual-key");
    expect(input).toHaveValue("BJ.manual-key");
    expect(screen.getByRole("button", { name: "获取模型" })).toBeEnabled();
  });

  it("keeps explicit local-client import as an optional path", async () => {
    const user = userEvent.setup();
    const onCredential = vi.fn();
    modelFetchMocks.importJoycodeCredential.mockResolvedValue({
      ptKey: "BJ.local-key",
      loginType: "N_PIN_PC",
      tenant: "joycode",
    });
    modelFetchMocks.fetchJoycodeModels.mockResolvedValue([
      {
        id: "joy-model",
        ownedBy: "joycode",
        wireApi: "responses",
      },
    ]);
    render(<ControlledFields onCredential={onCredential} />);

    await user.click(
      screen.getByRole("button", { name: "从 JoyCode 一键导入" }),
    );

    await waitFor(() =>
      expect(onCredential).toHaveBeenCalledWith("BJ.local-key"),
    );
    expect(modelFetchMocks.fetchJoycodeModels).toHaveBeenCalledWith({
      network: "internal",
      ptKey: "BJ.local-key",
      loginType: "N_PIN_PC",
      tenant: "joycode",
      externalBaseUrl: undefined,
    });
    expect(await screen.findByText("joy-model")).toBeInTheDocument();
  });

  it("shows the actionable backend reason when model discovery fails", async () => {
    const user = userEvent.setup();
    modelFetchMocks.fetchJoycodeModels.mockRejectedValue(
      "JoyCode 认证失败：账号未登录或 ptKey 已失效，请重新登录 JoyCode 并填写最新 ptKey",
    );
    render(<ControlledFields />);

    await user.type(screen.getByLabelText("JoyCode ptKey"), "BJ.expired-key");
    await user.click(screen.getByRole("button", { name: "获取模型" }));

    await waitFor(() =>
      expect(toastMocks.error).toHaveBeenCalledWith(
        expect.stringContaining("ptKey 已失效"),
      ),
    );
  });
});
