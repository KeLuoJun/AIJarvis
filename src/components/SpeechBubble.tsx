import type { ChatFailureKind } from "../lib/types";
import type { ChatExchange } from "../hooks/useChatSession";

const failureCopy: Record<ChatFailureKind, string> = {
  modelUnavailable: "本地模型尚未就绪。",
  timeout: "本轮回复超时。",
  invalidResponse: "模型回复格式无效。",
  queueFull: "当前消息较多，请稍后再试。",
  internal: "本轮回复未能完成。",
};

export function SpeechBubble({
  exchange,
  compact = false,
}: {
  exchange: ChatExchange;
  compact?: boolean;
}) {
  const response =
    exchange.status === "accepted"
      ? "正在思考..."
      : exchange.status === "replied"
        ? exchange.reply
        : exchange.status === "failed"
          ? failureCopy[exchange.failure ?? "internal"]
          : "本轮已取消。";

  return (
    <article
      className={`speech-bubble speech-bubble--${exchange.status} ${compact ? "speech-bubble--compact" : ""}`}
      aria-live={exchange.status === "accepted" ? "polite" : "off"}
    >
      <p className="speech-bubble__user">{exchange.userMessage}</p>
      <p className="speech-bubble__reply">{response}</p>
    </article>
  );
}
