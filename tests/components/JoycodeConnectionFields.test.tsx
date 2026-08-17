import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { JoycodeConnectionFields } from "@/components/providers/forms/JoycodeConnectionFields";

const modelFetchMocks = vi.hoisted(() => ({
  discoverJoycodePtKey: vi.fn(),
  fetchJoycodeModels: vi.fn(),
}));

vi.mock("@/lib/api/model-fetch", () => ({
  discoverJoycodePtKey: modelFetchMocks.discoverJoycodePtKey,
  fetchJoycodeModels: modelFetchMocks.fetchJoycodeModels,
}));

vi.mock("sonner", () => ({
  toast: {
    error: vi.fn(),
    info: vi.fn(),
    success: vi.fn(),
  },
}));

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
    modelFetchMocks.discoverJoycodePtKey.mockResolvedValue("BJ.local-key");
    modelFetchMocks.fetchJoycodeModels.mockResolvedValue([
      {
        id: "joy-model",
        ownedBy: "joycode",
        wireApi: "responses",
      },
    ]);
    render(<ControlledFields onCredential={onCredential} />);

    await user.click(
      screen.getByRole("button", { name: "从本机 JoyCode 导入" }),
    );

    await waitFor(() =>
      expect(onCredential).toHaveBeenCalledWith("BJ.local-key"),
    );
    expect(modelFetchMocks.fetchJoycodeModels).toHaveBeenCalledWith({
      network: "internal",
      ptKey: "BJ.local-key",
    });
    expect(await screen.findByText("joy-model")).toBeInTheDocument();
  });
});
