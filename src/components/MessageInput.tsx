import { SendHorizontal } from "lucide-react";
import { useEffect, useState, type FormEvent, type KeyboardEvent } from "react";

import { noteUserActivity } from "../lib/ipc";

export function MessageInput({
  disabled = false,
  busy = false,
  compact = false,
  autoFocus = false,
  onSend,
}: {
  disabled?: boolean;
  busy?: boolean;
  compact?: boolean;
  autoFocus?: boolean;
  onSend: (message: string) => Promise<boolean>;
}) {
  const [message, setMessage] = useState("");
  const [focused, setFocused] = useState(false);
  useEffect(() => {
    if (!focused || disabled) return;
    const notify = () => { void noteUserActivity().catch(() => {}); };
    notify();
    const timer = setInterval(notify, 5_000);
    return () => clearInterval(timer);
  }, [disabled, focused]);
  const canSend = !disabled && !busy && message.trim().length > 0;

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!canSend) return;
    if (await onSend(message)) setMessage("");
  };

  const submitOnEnter = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== "Enter" || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    event.currentTarget.form?.requestSubmit();
  };

  return (
    <form
      className={`message-input ${compact ? "message-input--compact" : ""}`}
      onSubmit={(event) => void submit(event)}
    >
      <textarea
        aria-label="发送文字消息"
        autoFocus={autoFocus}
        maxLength={4000}
        rows={compact ? 1 : 2}
        placeholder="对 Jarvis 说点什么"
        value={message}
        disabled={disabled}
        onChange={(event) => setMessage(event.currentTarget.value)}
        onFocus={() => setFocused(true)}
        onBlur={() => setFocused(false)}
        onKeyDown={submitOnEnter}
      />
      <button
        type="submit"
        disabled={!canSend}
        title="发送消息"
        aria-label={busy ? "消息发送中" : "发送消息"}
      >
        <SendHorizontal size={17} aria-hidden="true" />
      </button>
    </form>
  );
}
