# musher

**AI 코딩 에이전트 팀을 몰고 가는 도구.** 에이전트마다 독립된 git worktree를 주고, 태스크는 의존성 그래프(DAG) 순서로 실행합니다. 결과는 매번 여러분의 테스트로 검증하고, 실패하면 통과할 때까지 같은 에이전트에게 되돌려 고치게 합니다. 모든 과정은 영구 로그에 남습니다. 서버 없이 ~700KB 바이너리 하나로 동작합니다.

[English README](README.md)

```
musher run 261027-1102  3/5 done  2 active  $0.41  04:12
 ✓ api        done      01:40  $0.12  4 files  check ✓
 ✓ schema     done      00:52  $0.06  2 files  check ✓  2 attempts
 ◐ client     working   01:05  $0.09  Edit sdk/health.ts
 ◑ docs       checking  00:31  $0.03
 ◌ e2e        queued                 after client
```

**Herder(목동)**는 양떼를 지켜보는 사람이고, **Musher(머셔)**는 썰매견 팀을 몰아 목적지까지 데려가는 사람입니다. Herdr가 에이전트들이 *뭘 하는지 보여준다면*, Musher는 에이전트들과 함께 *일을 끝냅니다*.

---

## 왜 만들었나

여러 에이전트를 동시에 돌리면 늘 비슷한 문제가 생깁니다.

| 문제 | musher의 해결 |
|---|---|
| 두 에이전트가 같은 폴더를 고치다 서로 덮어씀 | 태스크마다 **git worktree + 브랜치**를 자동으로 생성하고 정리 |
| "B는 A 코드가 있어야 하는데…" | `after = ["a"]`로 지정하면 B는 **A의 브랜치 위에서 시작**하고, A의 결과 요약을 프롬프트로 전달받음 |
| 에이전트는 "완료"라는데 테스트는 빨간불 | `check = "npm test"`가 **관문** 역할. 실패 출력을 **같은 세션**에 되돌려 재시도 |
| 누가 막혔는지 화면을 보고 추측 | 화면 스크래핑이 아니라 에이전트의 **구조화 이벤트 스트림**(Claude `stream-json`, Codex `--json`)으로 판정 |
| 에이전트 간 맥락 단절 | **팀 공유 메모**(`musher mem`)와 **상류 결과 인계**를 모든 프롬프트에 주입 |
| "밤새 무슨 일이 있었지?" | **추가 전용 이벤트 로그**와 태스크별 로그를 디스크에 보존. 크래시, Ctrl-C, 계획 수정 후에도 `musher resume`으로 이어서 실행 |
| 터미널을 지키고 있어야 함 | `--bg`로 띄워 두고 어느 터미널에서든 `musher watch`, 스크립트에서는 `musher wait`. 완료되면 데스크톱 알림 |
| 비용 폭주 | 태스크·런별 **비용 추적**과 `budget_usd` 상한 |

## 설치

```sh
# 사전 빌드 바이너리 (macOS / Linux, arm64 / x86_64)
curl -fsSL https://raw.githubusercontent.com/MUSE-CODE-SPACE/musher/main/install.sh | sh

# 또는 소스에서
cargo install --git https://github.com/MUSE-CODE-SPACE/musher
```

필요 조건: `git`, 그리고 에이전트 CLI 하나 이상(Claude Code, Codex CLI, Gemini CLI). 다른 CLI도 `[agents.*]`로 연결할 수 있습니다. 환경은 `musher doctor`로 점검하세요.

## 60초 투어

```sh
cd your-repo
musher init                 # musher.toml 생성, .musher/를 git에서 숨김
$EDITOR musher.toml         # 태스크 작성
musher run                  # 라이브 대시보드, Ctrl-C로 안전하게 중단
musher diff api             # 태스크 하나의 고유 변경분 리뷰
musher merge                # 완료된 브랜치를 의존성 순서대로 병합
```

계획 파일 없이 바로 시킬 수도 있습니다.

```sh
musher do "utils/의 깨지는 날짜 테스트 고쳐줘" --check "npm test -- utils"
musher do "/login에 rate limit 추가" --agent codex --bg
```

## musher.toml

```toml
[settings]
parallel   = 3            # 동시에 실행할 태스크 수
agent      = "claude"     # claude | codex | gemini | shell | 직접 정의
isolation  = "worktree"   # "none"이면 저장소에서 직접 작업
timeout    = "30m"        # 시도당 제한 시간
retries    = 2            # 검증 실패 후 수정 시도 횟수
permission = "acceptEdits"
setup      = "npm ci"     # 새 worktree마다 1회 실행
budget_usd = 5.0          # 이 금액을 넘으면 새 태스크를 시작하지 않음
allow      = ["Bash(npm run lint:*)"]   # 추가로 허용할 도구 (각 태스크의 check 명령은 항상 허용)

[[task]]
id     = "api"
prompt = "GET /health 엔드포인트를 테스트와 함께 추가"
check  = "npm test"

[[task]]
id     = "client"
after  = ["api"]          # api 브랜치 위에 쌓고, api 요약을 전달받음
prompt = "SDK에 healthCheck() 추가"
check  = "npm test"
model  = "sonnet"         # 태스크별 덮어쓰기: agent, model, timeout, retries, permission, allow
```

설정 파일에 오타가 있으면 조용히 무시하지 않고 에러를 냅니다. 순환 의존성이나 존재하지 않는 id도 실행 전에 잡아냅니다. `musher run --dry-run`을 실행하면 병렬로 실행될 단계(wave)를 미리 볼 수 있습니다.

## 태스크 생명주기

```
queued ─▶ setup ─▶ working ─▶ (자동 커밋) ─▶ checking ─┬─▶ done
            │                                            │
            │       fixing ◀── 검증 출력 ◀───────────────┘  (최대 retries회)
            │         └─ 같은 에이전트 세션(--resume)이라 이전 시도를 기억함
            └─ 베이스 커밋에서 worktree 생성 → 의존 브랜치 병합 → setup 실행
```

* **failed**: 에이전트 오류, 시간 초과, 또는 재시도를 모두 써도 검증 실패
* **blocked**: 검증이 실패하는데 에이전트가 요청한 도구 권한이 거부된 경우. `musher attach <task>`로 들어가 권한을 주면 됨
* **skipped**: 의존 태스크가 완료되지 못함 (사유 표시)
* **stopped**: Ctrl-C로 중단. `musher resume`을 실행하면 완료된 작업은 그대로 두고 이 지점부터 이어서 진행. 그 사이에 `musher.toml`의 프롬프트를 고쳤다면 수정본이 반영됨

## 명령어

| | |
|---|---|
| `musher run [-j N] [--only a,b] [--dry-run] [--bg]` | 계획 실행 (`--only`는 의존 태스크도 함께 포함) |
| `musher do "<프롬프트>" [--agent] [--check] [--id] [--model] [--bg]` | 일회성 태스크 |
| `musher resume [run]` | 중단·실패한 런 이어서 실행 |
| `musher watch` / `status [--json]` / `wait` / `ls` | 어디서든 관찰. `wait`는 전부 done일 때만 exit 0 |
| `musher logs <task> [-f] [--raw]` | 읽기 쉬운 트랜스크립트(사용한 도구, 답변, 권한 거부)와 검증 출력 |
| `musher events [-f] [--json]` | 영구 이벤트 로그 |
| `musher diff <task> [--stat] [--all]` | 태스크 고유 변경분 (`--all`은 쌓인 의존 변경분 포함) |
| `musher ask <task> "<메시지>"` | 같은 세션에 후속 지시 → 재검증 |
| `musher attach <task>` | 해당 worktree에서 세션에 **대화형으로** 접속 |
| `musher merge [task…]` | 완료된 브랜치를 의존성 순서로 `--no-ff` 병합, 충돌 시 멈춤 |
| `musher mem ["메모"] [--edit] [--clear]` | 모든 프롬프트에 주입되는 팀 공유 메모 |
| `musher clean [--purge]` | worktree 정리 (`--purge`는 브랜치와 기록까지 삭제) |
| `musher doctor` | git과 설치된 에이전트 점검 |

### 에이전트를 리더로 쓰기

모든 명령은 스크립트로 호출할 수 있고, 상태는 JSON으로 받을 수 있습니다. 그래서 "리더" 에이전트가 직접 작업을 나눠 맡길 수 있습니다.

```sh
musher do "마이그레이션 작성" --id migrate --bg
musher wait --json | jq '.tasks[] | {id, status, note}'
```

Claude Code용 스킬은 [`skills/musher/SKILL.md`](skills/musher/SKILL.md)에 있습니다. `~/.claude/skills/musher/`에 복사해서 쓰세요.

## Musher vs Herdr

[Herdr](https://herdr.dev)는 훌륭한 에이전트 인식형 **터미널 멀티플렉서**입니다. tmux에 각 에이전트가 working/blocked/done 중 어떤 상태인지 보여주는 사이드바를 붙인 형태입니다. Musher는 **오케스트레이터**입니다. 두 도구는 계층이 다르기 때문에, 각자 무엇에 강한지를 그대로 적었습니다.

| | **Herdr** | **musher** |
|---|---|---|
| 핵심 개념 | 대화형 에이전트 터미널 다중화 | 에이전트 태스크 그래프를 검증된 결과까지 실행 |
| 실행 모델 | 백그라운드 서버가 PTY 소유 | 서버 없음: 런 = 프로세스 + `.musher/` 파일 |
| 바이너리 | ~10–11 MB¹ | **~0.7 MB** (실측 705 KB, macOS arm64) |
| 유휴 비용 | 서버 상주 | **아무것도 안 돎** |
| 에이전트 8개 병렬 오케스트레이션 중 | — | **RSS 2.8 MB** (실측), 기동 2.4 ms |
| 상태 판정 | 프로세스명 + **화면 패턴 휴리스틱**, 확신 없으면 `unknown`² | 에이전트의 **구조화 이벤트**(세션 id, 도구, 비용, 최종 답, 권한 거부) + 종료 코드 |
| 에이전트 간 격리 | 없음 (같은 체크아웃 공유)² | 태스크별 **worktree + 브랜치** 자동 |
| 의존성/태스크 그래프 | 제공 안 함² | `after` DAG, 병렬 웨이브, **브랜치 스태킹**, 실패 전파 |
| 검증 | 제공 안 함 | `check` 관문 + 같은 세션에서 **자동 수정 루프** |
| 공유 맥락 | 파일/git으로만² | 팀 메모 + 상류 요약 프롬프트 주입 |
| 기록 | 실시간 상태만, 영구 이벤트 기록 없음² | **JSONL 이벤트**, 태스크 로그, 런 스냅샷 |
| 크래시/재시작 | 레이아웃 복원, 대화는 세션 id 기록분만² | `musher resume`: 완료분 유지, 세션 재개 |
| 비용 통제 | — | 태스크/런 비용, `budget_usd` |
| 결과 반영 | 수동 | `musher diff`, `musher merge` (의존성 순) |
| 에이전트 화면 실시간 보기, 마우스, 분할 | **예 (Herdr의 강점)** | 멀티플렉서 아님: `logs -f`, `watch`, `attach` |
| 원격 씬 클라이언트, 플러그인 마켓 | **예** | 없음 (CLI이므로 SSH로 사용) |

¹ 서드파티 리뷰([bitdoze](https://www.bitdoze.com/herdr-agent-multiplexer/)) ² Herdr 문서와 [Developers Digest 심층 분석](https://www.developersdigest.tech/blog/herdr-deep-dive-agent-terminal-multiplexer)에 명시된 한계. Herdr 수치는 2026년 10월 기준 공개 자료이고, musher 수치는 Apple Silicon Mac에서 직접 측정했습니다.

**Herdr가 맞는 경우**: 에이전트 5개의 화면을 나란히 띄워 놓고 직접 조종하고 싶을 때. **musher가 맞는 경우**: 할 일 목록을 맡기고 자리를 비운 뒤, 돌아와서 테스트를 통과한 격리 브랜치를 받고 싶을 때. 둘을 함께 써도 됩니다. Herdr 패널 하나에서 `musher watch`를 띄우면 됩니다.

## 설계 원칙

* **일부러 작게.** Rust 약 2.7k줄, 의존성은 `serde`, `serde_json`, `toml`, `libc`뿐입니다. async 런타임도 TUI 프레임워크도 쓰지 않습니다.
* **파일이 곧 API.** `state.json`은 원자적으로 교체되고, `events.jsonl`은 추가만 하며, `logs/`와 `out/`은 평문입니다. 어떤 도구로든 읽을 수 있습니다.
* **안전한 기본값.** 기본 권한은 `acceptEdits`이고, 에이전트는 절대 push하지 않습니다. `.musher/`는 `.git/info/exclude`로 숨기므로 `.gitignore`를 건드리지 않습니다. Ctrl-C를 누르면 에이전트 프로세스 그룹 전체를 종료하므로 고아 프로세스가 남지 않습니다.
* **지원 플랫폼:** macOS, Linux. Windows는 미검증입니다(WSL 권장).

## API 키 없이 체험

```sh
cd examples/demo && ./run-demo.sh
```

내장 `shell` 에이전트로 병렬 웨이브, 브랜치 스태킹, 검증 실패 후 자동 수정, 실패 전파, `resume`, `merge`를 모두 보여줍니다.

## 라이선스

MIT © Muse
