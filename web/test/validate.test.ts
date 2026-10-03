import { describe, expect, it } from "vitest";
import { MAX_PLAIN, budgetOf, checkEndpoint, emptyForm, isZipMagic, validate, type FormInput } from "../src/validate";

const ZIP = new Uint8Array([0x50, 0x4b, 0x03, 0x04]);

function agentForm(over: Partial<FormInput> = {}): FormInput {
  return {
    ...emptyForm("agent"),
    taskset: "github-full",
    file: { name: "agent.zip", size: 1000 },
    fileMagic: ZIP,
    model: "glm-5.3",
    endpoint: "https://api.example.com/v1",
    apiKey: "sk-x",
    replicas: "3",
    password: "123456789012",
    password2: "123456789012",
    consent: true,
    ...over,
  };
}

describe("validate", () => {
  it("accepts a complete agent form", () => {
    expect(validate(agentForm())).toEqual({});
  });

  it("requires consent", () => {
    expect(validate(agentForm({ consent: false })).consent).toBeTruthy();
  });

  it("checks the file", () => {
    expect(validate(agentForm({ file: null })).file).toBeTruthy();
    expect(validate(agentForm({ file: { name: "a.tar", size: 10 } })).file).toMatch(/zip/);
    expect(validate(agentForm({ file: { name: "a.zip", size: MAX_PLAIN + 1 } })).file).toMatch(/25 MB/);
    expect(validate(agentForm({ fileMagic: new Uint8Array([1, 2, 3, 4]) })).file).toBeTruthy();
    expect(isZipMagic(ZIP)).toBe(true);
  });

  it("checks replicas and budget", () => {
    expect(validate(agentForm({ replicas: "0" })).replicas).toBeTruthy();
    expect(validate(agentForm({ replicas: "11" })).replicas).toBeTruthy();
    expect(validate(agentForm({ replicas: "2.5" })).replicas).toBeTruthy();
    expect(validate(agentForm({ maxTokens: "abc" })).maxTokens).toBeTruthy();
    expect(validate(agentForm({ maxCostUsd: "-1" })).maxCostUsd).toBeTruthy();
    expect(validate(agentForm({ maxCostUsd: "12.5", maxRequests: "300" }))).toEqual({});
  });

  it("checks the download password", () => {
    expect(validate(agentForm({ password: "short", password2: "short" })).password).toBeTruthy();
    // At least 12 characters.
    expect(validate(agentForm({ password: "12345678901", password2: "12345678901" })).password).toBeTruthy();
    expect(validate(agentForm({ password: "123456789012", password2: "123456789012" }))).toEqual({});
    expect(validate(agentForm({ password: "电池电池电池电池电池电池", password2: "电池电池电池电池电池电池" }))).toEqual({});
    expect(validate(agentForm({ password2: "different" })).password2).toBeTruthy();
  });

  it("app mode needs a stage but no model fields", () => {
    const f: FormInput = {
      ...emptyForm("app"),
      taskset: "github-full",
      file: { name: "site.zip", size: 10 },
      fileMagic: ZIP,
      consent: true,
    };
    expect(validate(f)).toEqual({ stage: "请选择阶段" });
    expect(validate({ ...f, stage: "2" })).toEqual({});
  });

  it("budgetOf keeps only filled values", () => {
    expect(budgetOf(agentForm())).toBeUndefined();
    expect(budgetOf(agentForm({ maxRequests: "100", maxCostUsd: "2.5" }))).toEqual({
      max_requests: 100,
      max_cost_usd: 2.5,
    });
  });
});

describe("checkEndpoint", () => {
  it.each([
    ["https://api.example.com/v1", null],
    ["http://api.example.com/v1", /https/],
    ["not a url", /格式/],
    ["", /填写/],
    ["https://localhost:8080", /内网/],
    ["https://127.0.0.1/v1", /内网/],
    ["https://10.1.2.3/v1", /内网/],
    ["https://192.168.0.5/v1", /内网/],
    ["https://172.20.0.1/v1", /内网/],
    ["https://169.254.169.254/latest", /内网/],
    ["https://[::1]/v1", /内网/],
    ["https://user:pw@api.example.com", /用户名/],
  ])("%s", (url, expected) => {
    const r = checkEndpoint(url);
    if (expected === null) expect(r).toBeNull();
    else expect(r).toMatch(expected);
  });
});
