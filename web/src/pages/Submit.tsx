import type { ComponentChildren } from "preact";
import { useState } from "preact/hooks";
import { api } from "../api";
import { submitEval, STEP_TEXT, type Step } from "../submit";
import type { Mode, TaskSet } from "../types";
import { ErrorBox, Loading, useAsync } from "../ui";
import { CONSENT_TEXT, MAX_REPLICAS, MIN_PASSWORD, emptyForm, validate, type Errors, type FormInput } from "../validate";
import { fmtDuration } from "../stats";

function fmtSize(n: number): string {
  return n >= 1024 * 1024 ? `${(n / 1024 / 1024).toFixed(1)} MB` : `${Math.max(1, Math.round(n / 1024))} KB`;
}

function Field({
  label,
  error,
  hint,
  children,
  id,
}: {
  label: string;
  error?: string;
  hint?: ComponentChildren;
  children: ComponentChildren;
  id: string;
}) {
  return (
    <div class={`field ${error ? "has-error" : ""}`}>
      <label for={id}>{label}</label>
      {children}
      {hint && !error && <p class="hint">{hint}</p>}
      {error && (
        <p class="error" id={`${id}-err`}>
          {error}
        </p>
      )}
    </div>
  );
}

export function Submit() {
  const tasksets = useAsync(() => api().tasksets());
  const [mode, setMode] = useState<Mode>("agent");
  const [form, setForm] = useState<FormInput>(() => emptyForm("agent"));
  const [fileObj, setFileObj] = useState<File | null>(null);
  const [errors, setErrors] = useState<Errors>({});
  const [tried, setTried] = useState(false);
  const [step, setStep] = useState<Step | null>(null);
  const [failure, setFailure] = useState<Error | null>(null);
  const [done, setDone] = useState<{ eval_id: string; rotated: boolean } | null>(null);

  const set = <K extends keyof FormInput>(k: K, v: FormInput[K]) => {
    const next = { ...form, [k]: v };
    setForm(next);
    if (tried) setErrors(validate(next));
  };

  const switchMode = (m: Mode) => {
    setMode(m);
    const next = { ...form, mode: m };
    setForm(next);
    if (tried) setErrors(validate(next));
  };

  const onFile = async (e: Event) => {
    const f = (e.currentTarget as HTMLInputElement).files?.[0] ?? null;
    setFileObj(f);
    const magic = f ? new Uint8Array(await f.slice(0, 4).arrayBuffer()) : null;
    const next = { ...form, file: f ? { name: f.name, size: f.size } : null, fileMagic: magic };
    setForm(next);
    if (tried) setErrors(validate(next));
  };

  const ts: TaskSet | undefined = tasksets.data?.find((t) => t.name === form.taskset);

  const onSubmit = async (e: Event) => {
    e.preventDefault();
    setTried(true);
    const errs = validate(form);
    setErrors(errs);
    if (Object.keys(errs).length || !fileObj) {
      const first = document.querySelector<HTMLElement>(".has-error input, .has-error select");
      first?.focus();
      return;
    }
    setFailure(null);
    try {
      const bytes = new Uint8Array(await fileObj.arrayBuffer());
      const res = await submitEval(api(), form, bytes, setStep);
      // Drop secrets from memory as soon as they are sealed.
      setForm((f) => ({ ...f, apiKey: "", password: "", password2: "" }));
      setDone(res);
    } catch (err) {
      setFailure(err as Error);
    } finally {
      setStep(null);
    }
  };

  if (done) {
    return (
      <div class="stack">
        <h1>已提交</h1>
        {done.rotated && (
          <div class="notice warn">平台公钥已更换，本次已使用新公钥加密。请刷新页面以加载最新版本。</div>
        )}
        <p>
          评测编号 <code>{done.eval_id}</code>。排队后会自动开始，可在评测详情页查看进度。
        </p>
        <div class="row">
          <a class="btn primary" href={`#/evals/${done.eval_id}`}>
            查看评测
          </a>
          <button
            class="btn"
            onClick={() => {
              setDone(null);
              setForm(emptyForm(mode));
              setFileObj(null);
              setTried(false);
              setErrors({});
            }}
          >
            再提交一个
          </button>
        </div>
      </div>
    );
  }

  const busy = step !== null;
  const err = (k: keyof FormInput) => errors[k];
  const aria = (k: keyof FormInput) =>
    errors[k] ? { "aria-invalid": true, "aria-describedby": `f-${k}-err` } : {};

  return (
    <form class="stack" onSubmit={onSubmit} noValidate>
      <h1>提交评测</h1>

      <div class="seg" role="tablist" aria-label="提交方式">
        <button
          type="button"
          role="tab"
          aria-selected={mode === "agent"}
          class={mode === "agent" ? "on" : ""}
          onClick={() => switchMode("agent")}
        >
          上传 agent<span>完整评测</span>
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={mode === "app"}
          class={mode === "app" ? "on" : ""}
          onClick={() => switchMode("app")}
        >
          上传产出<span>快速打分</span>
        </button>
      </div>
      <p class="muted small">
        {mode === "agent"
          ? "平台用你的模型 key 按阶段运行 agent、逐阶段打分，给出每遍每阶段的分数、用时、token 和等价花销。"
          : "上传已经生成好的产出（如网站的 zip），只打分，几分钟出结果。"}
      </p>

      <fieldset class="card" disabled={busy}>
        <legend>题目</legend>
        <Field id="f-taskset" label="题目包" error={err("taskset")}>
          {tasksets.loading ? (
            <Loading />
          ) : tasksets.error ? (
            <ErrorBox error={tasksets.error} onRetry={tasksets.reload} />
          ) : (
            <select
              id="f-taskset"
              value={form.taskset}
              {...aria("taskset")}
              onChange={(e) => {
                const v = e.currentTarget.value;
                setForm((f) => {
                  const next = { ...f, taskset: v, stage: "" };
                  if (tried) setErrors(validate(next));
                  return next;
                });
              }}
            >
              <option value="">请选择</option>
              {tasksets.data!
                .filter((t) => !t.status || t.status === "ready")
                .map((t) => (
                  <option value={t.name}>
                    {t.title ? `${t.title}（上传，${t.public ? "公开" : "私有"}` : `${t.name}（v${t.version}`}，{t.stages.length} 个阶段）
                  </option>
                ))}
            </select>
          )}
        </Field>
        {mode === "app" && (
          <Field id="f-stage" label="阶段" error={err("stage")}>
            <select
              id="f-stage"
              value={form.stage}
              disabled={!ts}
              {...aria("stage")}
              onChange={(e) => set("stage", e.currentTarget.value)}
            >
              <option value="">{ts ? "请选择" : "先选择题目包"}</option>
              {ts?.stages.map((s, i) => (
                <option value={String(i + 1)}>
                  {s.name}（{s.total} 项测试）
                </option>
              ))}
            </select>
          </Field>
        )}
        {mode === "agent" && ts && (
          <p class="hint">
            共 {ts.stages.length} 个阶段，限时合计{" "}
            {fmtDuration(ts.stages.reduce((a, s) => a + s.time_limit_s, 0))}。
          </p>
        )}
      </fieldset>

      <fieldset class="card" disabled={busy}>
        <legend>{mode === "agent" ? "agent 包" : "产出"}</legend>
        <Field
          id="f-file"
          label={mode === "agent" ? "agent 包（zip）" : "产出（zip）"}
          error={err("file")}
          hint={
            mode === "agent"
              ? "包含 agent.json 和 Dockerfile。上传前在浏览器中加密，最大 25 MB。"
              : "上传前在浏览器中加密，最大 25 MB。"
          }
        >
          <label class="file-pick">
            <input id="f-file" type="file" accept=".zip,application/zip" onChange={onFile} {...aria("file")} />
            <span class="btn">选择文件</span>
            <span class="file-name">
              {form.file ? `${form.file.name}（${fmtSize(form.file.size)}）` : "未选择文件"}
            </span>
          </label>
        </Field>
      </fieldset>

      {mode === "agent" && (
        <fieldset class="card" disabled={busy}>
          <legend>模型</legend>
          <Field id="f-model" label="模型名" error={err("model")} hint="原样传给接口，如 glm-5.3。">
            <input
              id="f-model"
              type="text"
              autocomplete="off"
              spellcheck={false}
              value={form.model}
              {...aria("model")}
              onInput={(e) => set("model", e.currentTarget.value)}
            />
          </Field>
          <Field id="f-endpoint" label="接口地址" error={err("endpoint")} hint="OpenAI 兼容接口，必须是 https。">
            <input
              id="f-endpoint"
              type="url"
              inputMode="url"
              autocomplete="off"
              spellcheck={false}
              placeholder="https://api.example.com/v1"
              value={form.endpoint}
              {...aria("endpoint")}
              onInput={(e) => set("endpoint", e.currentTarget.value)}
            />
          </Field>
          <Field
            id="f-apiKey"
            label="API key"
            error={err("apiKey")}
            hint="在浏览器中加密后提交，服务器看不到明文；评测结束后立即删除。"
          >
            <input
              id="f-apiKey"
              type="password"
              autocomplete="off"
              spellcheck={false}
              value={form.apiKey}
              {...aria("apiKey")}
              onInput={(e) => set("apiKey", e.currentTarget.value)}
            />
          </Field>
        </fieldset>
      )}

      {mode === "agent" && (
        <fieldset class="card" disabled={busy}>
          <legend>运行</legend>
          <Field id="f-replicas" label="运行遍数" error={err("replicas")} hint={`1–${MAX_REPLICAS} 遍，多遍给出均值和波动。`}>
            <input
              id="f-replicas"
              type="number"
              min={1}
              max={MAX_REPLICAS}
              step={1}
              inputMode="numeric"
              value={form.replicas}
              {...aria("replicas")}
              onInput={(e) => set("replicas", e.currentTarget.value)}
            />
          </Field>
          <details class="budget">
            <summary>预算上限（可选，默认不限）</summary>
            <div class="grid3">
              <Field id="f-maxRequests" label="请求数" error={err("maxRequests")}>
                <input
                  id="f-maxRequests"
                  type="text"
                  inputMode="numeric"
                  value={form.maxRequests}
                  {...aria("maxRequests")}
                  onInput={(e) => set("maxRequests", e.currentTarget.value)}
                />
              </Field>
              <Field id="f-maxTokens" label="token 数" error={err("maxTokens")}>
                <input
                  id="f-maxTokens"
                  type="text"
                  inputMode="numeric"
                  value={form.maxTokens}
                  {...aria("maxTokens")}
                  onInput={(e) => set("maxTokens", e.currentTarget.value)}
                />
              </Field>
              <Field id="f-maxCostUsd" label="美元" error={err("maxCostUsd")}>
                <input
                  id="f-maxCostUsd"
                  type="text"
                  inputMode="decimal"
                  value={form.maxCostUsd}
                  {...aria("maxCostUsd")}
                  onInput={(e) => set("maxCostUsd", e.currentTarget.value)}
                />
              </Field>
            </div>
            <p class="hint">达到任一上限后，后续模型请求会被拒绝。</p>
          </details>
        </fieldset>
      )}

      {mode === "agent" && (
        <fieldset class="card" disabled={busy}>
          <legend>取回产出</legend>
          <Field
            id="f-password"
            label="下载密码"
            error={err("password")}
            hint={
              <>
                产出会打包成 AES-256 加密的 zip，用此密码解压。需用 <b>7-Zip</b>、<b>Keka</b> 或{" "}
                <b>The Unarchiver</b>，系统自带的解压工具打不开。平台不保存此密码，忘记无法找回。
                <br />
                至少 {MIN_PASSWORD} 个字符。产出 zip 存放在公开位置，只靠此密码保护，请使用强密码。
              </>
            }
          >
            <input
              id="f-password"
              type="password"
              autocomplete="new-password"
              value={form.password}
              {...aria("password")}
              onInput={(e) => set("password", e.currentTarget.value)}
            />
          </Field>
          <Field id="f-password2" label="再次输入下载密码" error={err("password2")}>
            <input
              id="f-password2"
              type="password"
              autocomplete="new-password"
              value={form.password2}
              {...aria("password2")}
              onInput={(e) => set("password2", e.currentTarget.value)}
            />
          </Field>
        </fieldset>
      )}

      <fieldset class="card" disabled={busy}>
        <legend>公开与声明</legend>
        <label class="switch">
          <input
            type="checkbox"
            role="switch"
            checked={form.scorePublic}
            onChange={(e) => set("scorePublic", e.currentTarget.checked)}
          />
          <span>
            公开分数
            <small class="muted">公开分数和统计数据；产出和日志始终不公开。</small>
          </span>
        </label>
        <label class={`check ${err("consent") ? "has-error" : ""}`}>
          <input
            type="checkbox"
            checked={form.consent}
            {...aria("consent")}
            onChange={(e) => set("consent", e.currentTarget.checked)}
          />
          <span>
            我同意：{CONSENT_TEXT}
          </span>
        </label>
        {err("consent") && (
          <p class="error" id="f-consent-err">
            {err("consent")}
          </p>
        )}
      </fieldset>

      {failure && <ErrorBox error={failure} />}
      {tried && Object.keys(errors).length > 0 && (
        <p class="error" role="alert">
          还有 {Object.keys(errors).length} 处需要修改。
        </p>
      )}

      <div class="row">
        <button type="submit" class="btn primary wide" disabled={busy}>
          {busy ? STEP_TEXT[step!] : "加密并提交"}
        </button>
      </div>
    </form>
  );
}
