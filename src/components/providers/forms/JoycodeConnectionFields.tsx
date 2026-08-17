import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import { settingsApi } from "@/lib/api";
import {
  discoverJoycodePtKey,
  fetchJoycodeModels,
  type JoycodeFetchedModel,
} from "@/lib/api/model-fetch";

export type JoycodeNetwork = "internal" | "external";

interface JoycodeConnectionFieldsProps {
  network: JoycodeNetwork;
  onNetworkChange: (network: JoycodeNetwork) => void;
  onCredential: (ptKey: string) => void;
}

// The reference client only defines the official product address. Do not
// invent a private login path; the official site owns its current login route.
const JOYCODE_LOGIN_URL = "http://joycode.jd.com";

const wait = (milliseconds: number) =>
  new Promise<void>((resolve) => window.setTimeout(resolve, milliseconds));

/**
 * JoyCode 的网页登录不会把浏览器 Cookie 暴露给 CC Switch。这里打开官方
 * 登录页，并在后台只读检测 JoyCode/JoyCoder 官方客户端写入的本机凭据。
 */
export function JoycodeConnectionFields({
  network,
  onNetworkChange,
  onCredential,
}: JoycodeConnectionFieldsProps) {
  const { t } = useTranslation();
  const [detecting, setDetecting] = useState(false);
  const [loadingModels, setLoadingModels] = useState(false);
  const [credential, setCredential] = useState("");
  const [models, setModels] = useState<JoycodeFetchedModel[]>([]);
  const mountedRef = useRef(true);

  useEffect(
    () => () => {
      mountedRef.current = false;
    },
    [],
  );

  const loadModels = async (ptKey = credential) => {
    if (!ptKey) return;
    setLoadingModels(true);
    try {
      const catalog = await fetchJoycodeModels({
        network,
        ptKey,
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
      toast.error(
        t("providerForm.fetchModelsFailed", {
          defaultValue: "获取模型列表失败",
        }),
      );
    } finally {
      if (mountedRef.current) setLoadingModels(false);
    }
  };

  const detectCredential = async (openLoginPage: boolean) => {
    if (openLoginPage) {
      await settingsApi.openExternal(JOYCODE_LOGIN_URL);
    }
    setDetecting(true);
    try {
      // 登录完成时间不可预测；最多等待两分钟，期间不读取浏览器 Cookie。
      for (let attempt = 0; attempt < 60 && mountedRef.current; attempt += 1) {
        const ptKey = await discoverJoycodePtKey();
        if (ptKey) {
          setCredential(ptKey);
          onCredential(ptKey);
          toast.success(
            t("joycode.credentialImported", {
              defaultValue: "已从 JoyCode 官方客户端导入认证凭据",
            }),
          );
          await loadModels(ptKey);
          return;
        }
        if (!openLoginPage) break;
        await wait(2000);
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

      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="outline"
          disabled={detecting}
          onClick={() => void detectCredential(true)}
        >
          {detecting
            ? t("joycode.waitingForLogin", { defaultValue: "等待登录…" })
            : t("joycode.loginAndImport", {
                defaultValue: "打开 JoyCode 登录并自动导入",
              })}
        </Button>
        <Button
          type="button"
          variant="ghost"
          disabled={detecting}
          onClick={() => void detectCredential(false)}
        >
          {t("joycode.detectLocalCredential", {
            defaultValue: "检测本机凭据",
          })}
        </Button>
        <Button
          type="button"
          variant="ghost"
          disabled={detecting || loadingModels || !credential}
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
            "认证值只写入当前供应商配置；CC Switch 不读取浏览器 Cookie，也不会在界面中显示完整凭据。",
        })}
      </p>
    </section>
  );
}
