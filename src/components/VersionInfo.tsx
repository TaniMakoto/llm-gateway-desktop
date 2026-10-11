import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Copy } from "lucide-react";
import type { BuildInfo } from "@/gateway/types";

export function VersionInfo({
  onCopy,
}: {
  onCopy: (text: string, label: string) => void;
}) {
  const [info, setInfo] = useState<BuildInfo | null>(null);

  useEffect(() => {
    let active = true;
    void invoke<BuildInfo>("get_build_info")
      .then((value) => {
        if (active) setInfo(value);
      })
      .catch((error) => console.warn("读取程序版本失败", error));
    return () => {
      active = false;
    };
  }, []);

  if (!info) return null;

  const builtAt = /^\d+$/.test(info.builtAt)
    ? new Date(Number(info.builtAt) * 1000).toISOString()
    : info.builtAt;
  const summary = `LLM Gateway Desktop ${info.version}\n提交: ${info.commit}\n编译时间: ${builtAt}`;

  return (
    <div className="mb-5 flex items-center justify-between gap-3 text-xs text-muted-foreground">
      <span title={summary}>
        程序版本 {info.version} · {info.commit.slice(0, 8)}
      </span>
      <button
        className="secondary-button"
        onClick={() => onCopy(summary, "版本信息")}
      >
        <Copy className="h-3.5 w-3.5" />
        复制版本信息
      </button>
    </div>
  );
}
