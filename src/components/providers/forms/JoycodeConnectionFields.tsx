import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  discoverJoycodePtKey,
  fetchJoycodeModels,
  type JoycodeFetchedModel,
} from "@/lib/api/model-fetch";
import { extractErrorMessage } from "@/utils/errorUtils";

export type JoycodeNetwork = "internal" | "external";

interface JoycodeConnectionFieldsProps {
  network: JoycodeNetwork;
  onNetworkChange: (network: JoycodeNetwork) => void;
  credential: string;
  onCredential: (ptKey: string) => void;
}

/**
 * JoyCode 官网没有向第三方桌面应用提供认证回调。认证值由用户手动填写，
 * 也可以显式触发一次官方 JoyCode/JoyCoder 客户端本机凭据检测。
 */
export function JoycodeConnectionFields({
  network,
  onNetworkChange,
  credential,
  onCredential,
}: JoycodeConnectionFieldsProps) {
  const { t } = useTranslation();
  const [detecting, setDetecting] = useState(false);
  const [loadingModels, setLoadingModels] = useState(false);
  const [models, setModels] = useState<JoycodeFetchedModel[]>([]);
  const mountedRef = useRef(true);

  useEffect(
    () => () => {
      mountedRef.current = false;
    },
    [],
  );

  const loadModels = async (ptKey = credential.trim()) => {
    const normalizedPtKey = ptKey.trim();
    if (!normalizedPtKey) return;
    setLoadingModels(true);
    try {
      const catalog = await fetchJoycodeModels({
        network,
        ptKey: normalizedPtKey,
      });
      setModels(catalog);
      toast.success(
        t("providerForm.fetchModelsSuccess", {
          count: catalog.length,
          defaultValue: `已获取 ${catalog.length} 个模型`,
        }),
      );
    } catch (error) {
      console.warn("[JoyCode] model discovery failed", error);
      const title = t("providerForm.fetchModelsFailed", {
        defaultValue: "获取模型列表失败",
      });
      const detail = extractErrorMessage(error);
      toast.error(detail ? `${title}：${detail}` : title);
    } finally {
      if (mountedRef.current) setLoadingModels(false);
    }
  };

  const detectCredential = async () => {
    setDetecting(true);
    try {
      const ptKey = await discoverJoycodePtKey();
      if (ptKey) {
        onCredential(ptKey);
        toast.success(
          t("joycode.credentialImported", {
            defaultValue: "已从 JoyCode 官方客户端导入认证凭据",
          }),
        );
        await loadModels(ptKey);
        return;
      }
      if (mountedRef.current) {
        toast.info(
          t("joycode.credentialNotFound", {
            defaultValue:
              "未发现本机 JoyCode 凭据，请先在 JoyCode/JoyCoder 官方客户端完成登录",
          }),
        );
      }
    } catch (error) {
      console.warn("[JoyCode] credential discovery failed", error);
      toast.error(
        t("joycode.credentialImportFailed", {
          defaultValue: "JoyCode 凭据检测失败",
        }),
      );
    } finally {
      if (mountedRef.current) setDetecting(false);
    }
  };

  return (
    <section className="space-y-3 rounded-lg border border-border/60 bg-muted/20 p-4">
      <div className="space-y-2">
        <Label htmlFor="joycode-network">
          {t("joycode.network", { defaultValue: "JoyCode 网络地址" })}
        </Label>
        <select
          id="joycode-network"
          value={network}
          onChange={(event) => {
            setModels([]);
            onNetworkChange(event.target.value as JoycodeNetwork);
          }}
          className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
        >
          <option value="internal">
            {t("joycode.networkInternal", { defaultValue: "内网地址" })}
          </option>
          <option value="external">
            {t("joycode.networkExternal", { defaultValue: "外网地址" })}
          </option>
        </select>
        {network === "external" && (
          <p className="text-xs text-amber-600 dark:text-amber-400">
            {t("joycode.externalUnavailable", {
              defaultValue:
                "当前参考协议未提供可信的官方外网网关；该地址由部署方统一下发，用户无需填写。",
            })}
          </p>
        )}
      </div>

      <div className="space-y-2">
        <Label htmlFor="joycode-pt-key">
          {t("joycode.ptKey", { defaultValue: "JoyCode ptKey" })}
        </Label>
        <Input
          id="joycode-pt-key"
          type="password"
          autoComplete="off"
          value={credential}
          onChange={(event) => {
            setModels([]);
            onCredential(event.target.value.trim());
          }}
          placeholder={t("joycode.ptKeyPlaceholder", {
            defaultValue: "手动粘贴 JoyCode ptKey",
          })}
        />
        <p className="text-xs text-muted-foreground">
          {t("joycode.manualCredentialHint", {
            defaultValue:
              "JoyCode 官网登录不会向 CC Switch 回传认证值，请手动填写 ptKey；出现 401 时需在官方客户端重新登录并复制最新值。",
          })}
        </p>
      </div>

      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="outline"
          disabled={detecting}
          onClick={() => void detectCredential()}
        >
          {detecting
            ? t("joycode.detectingCredential", {
                defaultValue: "检测本机凭据…",
              })
            : t("joycode.detectLocalCredential", {
                defaultValue: "从本机 JoyCode 导入",
              })}
        </Button>
        <Button
          type="button"
          variant="ghost"
          disabled={detecting || loadingModels || !credential.trim()}
          onClick={() => void loadModels()}
        >
          {loadingModels
            ? t("joycode.loadingModels", { defaultValue: "获取模型中…" })
            : t("providerForm.fetchModels", { defaultValue: "获取模型" })}
        </Button>
      </div>
      {models.length > 0 && (
        <div className="flex max-h-24 flex-wrap gap-1 overflow-y-auto">
          {models.map((model) => (
            <span
              key={model.id}
              title={model.wireApi}
              className="rounded bg-muted px-2 py-1 text-xs text-muted-foreground"
            >
              {model.id}
            </span>
          ))}
        </div>
      )}
      <p className="text-xs text-muted-foreground">
        {t("joycode.loginSecurityHint", {
          defaultValue:
            "认证值只写入当前供应商配置；CC Switch 不读取浏览器 Cookie，也不会显示完整凭据。",
        })}
      </p>
    </section>
  );
}
