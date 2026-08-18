import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  fetchJoycodeModels,
  importJoycodeCredential,
  validateJoycodeCredential,
  type JoycodeCredential,
  type JoycodeFetchedModel,
} from "@/lib/api/model-fetch";
import { extractErrorMessage } from "@/utils/errorUtils";

export type JoycodeNetwork = "internal" | "external";

export interface JoycodeCredentialMetadata {
  loginType?: string;
  tenant?: string;
  externalBaseUrl?: string;
}

interface JoycodeConnectionFieldsProps {
  network: JoycodeNetwork;
  onNetworkChange: (network: JoycodeNetwork) => void;
  credential: string;
  credentialMetadata?: JoycodeCredentialMetadata;
  onCredential: (ptKey: string, metadata?: JoycodeCredentialMetadata) => void;
}

/**
 * JoyCode supports explicit manual credentials and importing the current
 * official IDE login state. Both paths validate through userInfo before use.
 */
export function JoycodeConnectionFields({
  network,
  onNetworkChange,
  credential,
  credentialMetadata,
  onCredential,
}: JoycodeConnectionFieldsProps) {
  const { t } = useTranslation();
  const [importing, setImporting] = useState(false);
  const [validating, setValidating] = useState(false);
  const [loadingModels, setLoadingModels] = useState(false);
  const [models, setModels] = useState<JoycodeFetchedModel[]>([]);
  const mountedRef = useRef(true);

  useEffect(
    () => () => {
      mountedRef.current = false;
    },
    [],
  );

  const metadataFromCredential = (
    value: JoycodeCredential,
  ): JoycodeCredentialMetadata => ({
    loginType: value.loginType,
    tenant: value.tenant,
    externalBaseUrl: value.colorBaseUrl,
  });

  const loadModels = async (
    ptKey = credential.trim(),
    metadata = credentialMetadata,
  ) => {
    const normalizedPtKey = ptKey.trim();
    if (!normalizedPtKey) return;
    setLoadingModels(true);
    try {
      const catalog = await fetchJoycodeModels({
        network,
        externalBaseUrl: metadata?.externalBaseUrl,
        ptKey: normalizedPtKey,
        loginType: metadata?.loginType,
        tenant: metadata?.tenant,
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

  const importCredential = async () => {
    setImporting(true);
    try {
      const imported = await importJoycodeCredential();
      if (imported) {
        const metadata = metadataFromCredential(imported);
        onCredential(imported.ptKey, metadata);
        toast.success(
          t("joycode.credentialImported", {
            defaultValue: "已从 JoyCode 官方客户端导入认证凭据",
          }),
        );
        await loadModels(imported.ptKey, metadata);
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
      console.warn("[JoyCode] credential import failed", error);
      const detail = extractErrorMessage(error);
      toast.error(
        detail ||
          t("joycode.credentialImportFailed", {
            defaultValue: "JoyCode 登录态导入失败",
          }),
      );
    } finally {
      if (mountedRef.current) setImporting(false);
    }
  };

  const validateManualCredential = async () => {
    const ptKey = credential.trim();
    if (!ptKey) return;
    setValidating(true);
    try {
      const validated = await validateJoycodeCredential({
        network,
        externalBaseUrl: credentialMetadata?.externalBaseUrl,
        ptKey,
        loginType: credentialMetadata?.loginType,
        tenant: credentialMetadata?.tenant,
      });
      const metadata = metadataFromCredential(validated);
      onCredential(validated.ptKey, metadata);
      toast.success(
        t("joycode.credentialValidated", {
          defaultValue: "JoyCode 认证验证成功",
        }),
      );
      await loadModels(validated.ptKey, metadata);
    } catch (error) {
      console.warn("[JoyCode] credential validation failed", error);
      const detail = extractErrorMessage(error);
      toast.error(
        detail ||
          t("joycode.credentialValidationFailed", {
            defaultValue: "JoyCode 认证验证失败",
          }),
      );
    } finally {
      if (mountedRef.current) setValidating(false);
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
        <p className="text-xs text-muted-foreground">
          {network === "external"
            ? t("joycode.externalAddressHint", {
                defaultValue: "使用 JoyCode 官方 HTTPS 网关，无需填写地址。",
              })
            : t("joycode.internalAddressHint", {
                defaultValue: "使用 JoyCode 内网服务地址，无需填写地址。",
              })}
        </p>
      </div>

      <div className="space-y-2 rounded-md border border-border/60 bg-background/60 p-3">
        <div className="text-sm font-medium">
          {t("joycode.importTitle", { defaultValue: "方式一：一键导入" })}
        </div>
        <p className="text-xs text-muted-foreground">
          {t("joycode.importHint", {
            defaultValue:
              "读取并验证本机 JoyCode IDE 的当前登录态；未登录时请先在 JoyCode 中完成网页登录。",
          })}
        </p>
        <Button
          type="button"
          variant="outline"
          disabled={importing || validating || loadingModels}
          onClick={() => void importCredential()}
        >
          {importing
            ? t("joycode.importingCredential", {
                defaultValue: "正在导入并验证…",
              })
            : t("joycode.importLocalCredential", {
                defaultValue: "从 JoyCode 一键导入",
              })}
        </Button>
      </div>

      <div className="space-y-3 rounded-md border border-border/60 bg-background/60 p-3">
        <div className="text-sm font-medium">
          {t("joycode.manualTitle", { defaultValue: "方式二：手动配置" })}
        </div>
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
            onCredential(event.target.value, {
              ...credentialMetadata,
              loginType: undefined,
              tenant: undefined,
            });
          }}
          placeholder={t("joycode.ptKeyPlaceholder", {
            defaultValue: "手动粘贴 JoyCode ptKey",
          })}
        />
        <div className="space-y-2">
          <Label htmlFor="joycode-login-type">
            {t("joycode.loginType", { defaultValue: "认证类型" })}
          </Label>
          <select
            id="joycode-login-type"
            value={credentialMetadata?.loginType ?? ""}
            onChange={(event) => {
              setModels([]);
              onCredential(credential, {
                ...credentialMetadata,
                loginType: event.target.value || undefined,
                tenant: undefined,
              });
            }}
            className="flex h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
          >
            <option value="">
              {t("joycode.loginTypeAuto", { defaultValue: "自动检测" })}
            </option>
            <option value="PIN_JD_CLOUD">PIN_JD_CLOUD</option>
            <option value="N_PIN_PC">N_PIN_PC</option>
            <option value="ERP">ERP</option>
          </select>
        </div>
        <Button
          type="button"
          variant="outline"
          disabled={
            importing || validating || loadingModels || !credential.trim()
          }
          onClick={() => void validateManualCredential()}
        >
          {validating
            ? t("joycode.validatingCredential", {
                defaultValue: "正在验证…",
              })
            : t("joycode.validateCredential", {
                defaultValue: "验证认证并获取模型",
              })}
        </Button>
      </div>

      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          variant="ghost"
          disabled={
            importing || validating || loadingModels || !credential.trim()
          }
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
