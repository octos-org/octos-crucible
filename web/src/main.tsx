import { render } from "preact";
import { useEffect, useState } from "preact/hooks";
import { API_BASE, ApiError, api, consumeTokenFromHash, getToken, isMock, loginUrl, setMock, setToken } from "./api";
import type { Me } from "./types";
import { Submit } from "./pages/Submit";
import { EvalList } from "./pages/EvalList";
import { EvalDetailPage } from "./pages/EvalDetail";
import { Tasksets } from "./pages/Tasksets";
import { ComparePage } from "./pages/Compare";
import { parseIds } from "./compare";
import "./style.css";

consumeTokenFromHash();

type Route =
  | { page: "submit" }
  | { page: "evals" }
  | { page: "eval"; id: string }
  | { page: "tasksets" }
  | { page: "compare"; ids: string[] };

function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, "");
  const m = path.match(/^\/evals\/([\w-]+)$/);
  if (m) return { page: "eval", id: m[1] };
  const c = path.match(/^\/compare(?:\?(.*))?$/);
  if (c) return { page: "compare", ids: parseIds(c[1] ?? "") };
  if (path === "/submit") return { page: "submit" };
  if (path === "/tasksets") return { page: "tasksets" };
  return { page: "evals" };
}

function useRoute(): Route {
  const [route, setRoute] = useState(() => parseRoute(location.hash));
  useEffect(() => {
    const on = () => {
      setRoute(parseRoute(location.hash));
      window.scrollTo(0, 0);
    };
    addEventListener("hashchange", on);
    return () => removeEventListener("hashchange", on);
  }, []);
  return route;
}

function Login() {
  const mock = isMock();
  return (
    <main class="login">
      <h1>octos-crucible</h1>
      <p class="lead">评测 coding agent：上传 agent 或产出，按阶段运行、打分，给出分数、用时、token 和等价花销。</p>
      {mock ? (
        <button
          class="btn primary"
          onClick={() => {
            setToken("mock-session");
            location.hash = "#/evals";
            location.reload();
          }}
        >
          用 GitHub 登录（演示）
        </button>
      ) : (
        <a class="btn primary" href={loginUrl()} aria-disabled={!API_BASE}>
          用 GitHub 登录
        </a>
      )}
      {!mock && !API_BASE && <p class="notice bad">未配置后端地址（VITE_API_BASE）。</p>}
      <p class="muted small">
        只读取你的 GitHub 用户名。不限次数使用。
        <a href="#/tasksets" class="link">查看题目包</a>
      </p>
    </main>
  );
}

function Shell() {
  const route = useRoute();
  const token = getToken();
  const [me, setMe] = useState<Me | null>(null);
  const [, setExpired] = useState(false);

  useEffect(() => {
    if (!token) return;
    api()
      .me()
      .then(setMe, (e) => {
        // A 401 has already cleared the token: re-render into the login page.
        if (e instanceof ApiError && e.status === 401) setExpired(true);
      });
  }, [token]);

  // Task sets are public; everything else needs a session.
  if (!token && route.page !== "tasksets") return <Login />;

  const nav = (href: string, label: string, active: boolean) => (
    <a href={href} class={active ? "active" : ""} aria-current={active ? "page" : undefined}>
      {label}
    </a>
  );

  return (
    <>
      <header class="top">
        <a href="#/evals" class="brand">
          crucible
        </a>
        <nav>
          {token && nav("#/submit", "提交", route.page === "submit")}
          {token && nav("#/evals", "我的评测", route.page === "evals" || route.page === "eval" || route.page === "compare")}
          {nav("#/tasksets", "题目包", route.page === "tasksets")}
        </nav>
        <div class="who">
          {me && <span class="muted small">{me.login}</span>}
          {token ? (
            <button
              class="link small"
              onClick={() => {
                setToken(null);
                location.hash = "";
                location.reload();
              }}
            >
              退出
            </button>
          ) : (
            <a class="link small" href="#/evals">
              登录
            </a>
          )}
        </div>
      </header>
      {isMock() && <div class="mockbar">演示模式：数据为固定样例，不会真正上传。</div>}
      <main class="page">
        {route.page === "submit" && <Submit />}
        {route.page === "evals" && <EvalList />}
        {route.page === "eval" && <EvalDetailPage id={route.id} />}
        {route.page === "tasksets" && <Tasksets />}
        {route.page === "compare" && <ComparePage ids={route.ids} />}
      </main>
    </>
  );
}

// Dev convenience: ?mock=1 / ?mock=0 toggles the mock backend for this tab.
const q = new URLSearchParams(location.search).get("mock");
if (q !== null && import.meta.env.DEV) setMock(q === "1");

render(<Shell />, document.getElementById("app")!);
