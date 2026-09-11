# dbops

*[English README](README.md)*

의존성 없는 단일 정적 바이너리로 OpenSearch/MongoDB/PostgreSQL/Redis를 점검·초기화하는
SRE/SE용 CLI. 서버에 `scp` 한 번으로 올리면 그 자리에서 `dbops pg health` 같은 점검이
끝난다 — 별도 런타임도, 공유 라이브러리도, `ca-certificates` 패키지 설치도 필요 없다.
`health` 커맨드는 nagios 호환 exit code(0/1/2/3)를 반환해 cron·NRPE·모니터링 에이전트에
바로 연결된다.

## 빠른 시작

```bash
# 설치 (또는 dist/의 바이너리를 서버에 scp)
curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh

# DB를 지정하고 점검
DBOPS_PG_HOST=pg01 DBOPS_PG_USER=dbops DBOPS_PG_PASSWORD=... dbops pg health
# PG HEALTH OK: primary, replica lag 0.3s | lag=0.3s;; connections=12;; max_connections=100;;

# 같은 점검을 기계 판독용으로, 모니터링은 exit code로 연결
dbops pg health --json; echo "exit=$?"
```

## 커맨드 트리

전역 플래그(모든 서브커맨드 공통): `--profile <NAME>` `--config <PATH>` `--json`
`--timeout <DUR>`(기본 5s) `--dry-run` `--yes` `--insecure` `-v/--verbose`

### os (OpenSearch/Elasticsearch 호환)

| 커맨드 | 설명 | 주요 플래그 |
|---|---|---|
| `os health` | 클러스터 상태 (nagios) | `--warning` `--critical` |
| `os nodes` | 노드별 디스크/힙 등 | |
| `os indices` | 인덱스 목록 | `--all` (시스템 인덱스 포함) |
| `os shards` | unassigned shard + 노드별 분포 | |
| `os stats` | 인덱스 통계 | `--index <PATTERN>` |
| `os init index <name>` | 인덱스 생성 | `--mapping <FILE>` `--if-not-exists` `--confirm-name` |
| `os reset index <name>` | drop + recreate (매핑 best-effort 보존) | `--confirm-name` |
| `os seed` | NDJSON bulk insert | `--index` `--file` `--confirm-name` |

### mongo (MongoDB)

| 커맨드 | 설명 | 주요 플래그 |
|---|---|---|
| `mongo health` | replica set 상태 (nagios) | `--warning` `--critical` |
| `mongo replset` | replSetGetStatus 상세 | |
| `mongo stats` | DB 통계 | `--db` |
| `mongo oplog` | oplog 윈도우 | |
| `mongo connections` | 커넥션 통계 | |
| `mongo init db <name>` | DB 생성 | `--confirm-name` |
| `mongo init user <name>` | 사용자 생성 | `--role` `--db` `--password` `--if-not-exists` `--confirm-name` |
| `mongo reset db <name>` | DB drop (재생성 없음) | `--confirm-name` |
| `mongo seed` | 컬렉션에 NDJSON/JSON 배열 삽입 | `--collection` `--db` `--file` `--confirm-name` |

### pg (PostgreSQL)

| 커맨드 | 설명 | 주요 플래그 |
|---|---|---|
| `pg health` | primary/standby lag (nagios) | `--warning` `--critical` |
| `pg stats` | DB 통계 | `--db` |
| `pg tables` | 큰 테이블 top N | `--top <N>` |
| `pg queries` | 장기 실행 쿼리 | `--long-running` `--threshold` |
| `pg vacuum` | vacuum/freeze 상태 | |
| `pg replication` | 복제 상태 | |
| `pg init schema` | SQL 파일 적용 (트랜잭션 래핑) | `--file` `--db` `--confirm-name` |
| `pg users list` | 역할 목록 + 소속 그룹 | |
| `pg users create <name>` | 역할 생성 | `--password-env` `--login`/`--no-login` `--if-not-exists` `--confirm-name` |
| `pg users grant <name>` | 역할 멤버십 + DB 권한 부여 | `--role` `--db` `--confirm-name` |
| `pg reset db <name>` | drop + recreate | `--confirm-name` |

`pg init schema`는 파일 전체를 하나의 트랜잭션으로 실행하므로, 중간에 실패하면 전부
롤백된다. 예외는 트랜잭션 안에서 실행할 수 없는 구문(예: `CREATE INDEX CONCURRENTLY`)이
들어 있는 경우다 — 이를 미리 감지해 구문을 하나씩 실행하고, 실패는 롤백 대신 몇 번째
구문에서 났는지로 보고한다.

### redis

| 커맨드 | 설명 | 주요 플래그 |
|---|---|---|
| `redis health` | PING 왕복시간 (nagios) | `--warning` `--critical` |
| `redis stats` | INFO 요약 | |
| `redis keyspace` | DB별 키 통계 | |
| `redis replication` | 복제 상태 | |
| `redis slowlog` | 느린 커맨드 목록 | `--n <N>` (기본 10) |

### 그 외

| 커맨드 | 설명 | 주요 플래그 |
|---|---|---|
| `http check <url>` | HTTP(S) 체크 + TLS 만료일 | `--expect-status` `--warning` `--critical` |
| `tcp check <host:port>` | TCP connect 체크 | `--warning` `--critical` |
| `sys check` | 로컬 디스크/메모리/로드/도커 컨테이너 수 | |
| `completion <shell>` | shell completion 스크립트 출력 (`bash`/`zsh`/`fish`/`powershell`/`elvish`) | |
| `update` | 최신 릴리스로 자기 자신을 교체 | `--tag <TAG>` `--force` (+ 전역 `--dry-run` `--json`) |

### `--warning` / `--critical` 단위

시간 접미사가 붙은 값(`500ms`, `5s`, `2m`)은 언제나 그 기간을 뜻한다. 접미사 없는 맨
숫자는 nagios 플러그인의 관례를 그대로 따라 커맨드마다 다르게 해석된다:

| 체크 | 맨 숫자의 의미 |
|---|---|
| `pg health`, `mongo health` | 복제 지연 초(seconds) |
| `redis health` | 응답 시간 밀리초(ms) |
| `os health` | unassigned shard 개수 |
| `http check`, `tcp check` | 응답/접속 시간 초(seconds) |

`http check`의 `--warning`/`--critical`은 응답 시간에만 적용된다. TLS 인증서 만료는 별도의
고정 임계값을 쓴다 — 남은 기간 30일이면 WARNING, 7일이면 CRITICAL.

### JSON 출력

`health`를 포함해 **모든** 커맨드가 `--json`을 지원한다 —
`dbops pg tables --json | jq .`도, `dbops pg health --json`도 그대로 파싱된다. 표 출력은
100행을 넘으면 "… N more rows"로 잘리지만, `--json`은 절대 잘리지 않는다.

## 설치

### install.sh (권장)

`install.sh`가 OS/아키텍처를 판별해 최신 릴리스에서 맞는 아티팩트를 받고, SHA256을
검증한 뒤 `dbops` 이름으로 설치한다.

```bash
curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh
```

| 환경변수 | 설명 | 기본값 |
|---|---|---|
| `DBOPS_VERSION` | 설치할 릴리스 태그 | 최신 릴리스 |
| `DBOPS_INSTALL_DIR` | 설치 위치 | 쓰기 가능하면 `/usr/local/bin`, 아니면 `~/.local/bin` |
| `DBOPS_REPO` | 받아올 `owner/name` | `x-mesh/dbops` |

앞의 두 값은 `--version` / `--dir` 플래그로도 줄 수 있다. curl과 wget 중 있는 쪽을 쓰고,
sha256 도구는 `sha256sum`/`shasum`/`openssl` 중 있는 걸 쓴다.

공개 저장소에서는 토큰이 필요 없다. 미인증 GitHub API 제한(IP당 시간당 60회)에 걸리거나
private 포크에서 설치할 때만 토큰을 준다 — 스크립트는 `DBOPS_GITHUB_TOKEN` →
`GITHUB_TOKEN` → `GH_TOKEN` 순으로 찾고, 셋 다 없으면 `gh auth token`까지 시도한다:

```bash
export GITHUB_TOKEN=$(gh auth token)   # 또는 PAT (contents: read)
curl -fsSL https://raw.githubusercontent.com/x-mesh/dbops/main/install.sh | sh
```

### `dbops update` — 설치 후 자체 업데이트

첫 설치 이후에는 바이너리가 스스로 갱신한다. 설치된 `dbops`를 원자적으로 교체하므로,
실행 중이던 프로세스가 있어도 안전하다.

```bash
dbops update                 # 최신 릴리스가 더 새로우면 교체
dbops update --dry-run       # 무엇을 할지만 출력, 파일은 건드리지 않음
dbops update --json          # {"action":"installed"|"up-to-date"|"planned", ...}
dbops update --tag v0.2.0    # 특정 릴리스로 고정 (다운그레이드도 허용)
dbops update --force         # 같은 버전이어도 다시 받아 덮어씀
```

동작은 install.sh와 같다 — 릴리스 조회 → 플랫폼에 맞는 아티팩트 다운로드 → 릴리스에 함께
올라간 `SHA256SUMS`와 대조 → 원자적 교체. 토큰도 같은 세 환경변수를 본다(`gh` 폴백은 없다.
서버에는 `gh`가 없으니까).

주의할 점 둘:

- **`/usr/local/bin`에 root 소유로 설치했다면 `sudo dbops update`가 필요하다.** 권한
  오류는 그 사실과 대안(`DBOPS_INSTALL_DIR=$HOME/.local/bin`)을 함께 알려준다.
- **전역 `--insecure`는 `update`에 적용되지 않는다.** 그 플래그는 self-signed 인증서를 쓰는
  DB에 붙기 위한 것이고, 자기 자신을 대체할 실행 파일을 받는 경로에서 인증서 검증을 끄는
  건 편의가 아니라 취약점이다.

SHA256 대조는 전송 중 손상/절단을 잡는 용도다. `SHA256SUMS`는 바이너리와 같은 릴리스에
들어 있으니 서명이 아니며, 릴리스의 진위는 api.github.com으로의 HTTPS가 담보한다.

`update`와 `http check`는 신뢰 루트를 바이너리에 내장(Mozilla CA 세트 + 호스트의 native
루트 union)해서 쓴다. 그래서 `ca-certificates` 패키지가 없는 최소 이미지(distroless, slim
Debian)에서도 TLS 검증이 그대로 동작하고, 동시에 호스트에 설치된 사내 CA(예: 인트라넷
엔드포인트를 `http check`할 때)도 인정한다. 자세한 배경은 `src/frame/tls.rs` 참고.

## 배포 (빌드 산출물 직접 다루기)

빌드 산출물은 CI가 태그 push 시 자동으로 만들거나(`.github/workflows/release.yml`),
로컬에서 `scripts/release-build.sh`로 만들 수 있다. 3타깃 전부 정적 바이너리(별도
런타임/라이브러리 설치 불필요)이며, `dist/dbops-<version>-<target>` 이름으로 나온다:

| 타깃 | 대상 환경 |
|---|---|
| `x86_64-unknown-linux-musl` | 대부분의 x86_64 리눅스 서버 (Alpine 포함, glibc 무관하게 동작) |
| `aarch64-unknown-linux-musl` | ARM64 리눅스 서버 |
| `aarch64-apple-darwin` (또는 빌드 호스트의 native 타깃) | 로컬 macOS 개발/터널링용 |

Intel macOS(`x86_64-apple-darwin`)용 아티팩트는 릴리스에 없다. install.sh와 `dbops update`
모두 그 호스트에서는 엉뚱한 바이너리를 내려받는 대신 "소스에서 빌드하라"고 멈춘다.
(Apple Silicon에서 Rosetta 셸로 실행한 경우는 `sysctl.proc_translated`로 구분해 arm64
아티팩트를 받는다.)

install.sh를 쓸 수 없는 폐쇄망이라면 서버 배포는 scp 1회로 끝난다:

```bash
# 1. scp로 올리고 실행권한 부여
scp dist/dbops-<version>-x86_64-unknown-linux-musl pg01:/usr/local/bin/dbops
ssh pg01 chmod +x /usr/local/bin/dbops

# 2. 의존성 없이 바로 동작하는지 증명 (동적 링크 라이브러리가 하나도 없어야 함)
ssh pg01 'ldd /usr/local/bin/dbops; dbops --version'
```

`ldd`가 "not a dynamic executable"(또는 동일 취지의 메시지)을 출력하면 정적 링크가 확인된
것 — glibc 버전 불일치, openssl 부재 등 흔한 배포 장애가 원천적으로 없다.

## 설정

접속 정보 우선순위는 **CLI 플래그 > `DBOPS_*` env var > TOML config 파일 > 내장 기본값**
순이다. (단, 현재 개별 DB 필드의 CLI 플래그는 아직 없다 — `--profile`/`--config`만 존재하고
나머지는 env/config로 병합된다. `src/frame/config.rs`의 `pick()` 병합 로직에 플래그 자리는
이미 마련되어 있어 나중에 필드별 플래그가 추가되어도 우선순위 규칙은 그대로 유지된다.)

`--profile`도 `DBOPS_PROFILE`도 config의 `default_profile`도 없으면 `default`라는 이름의
프로파일을 쓴다.

### config 파일 예시 (`~/.dbops.toml` 또는 `--config`로 지정)

```toml
default_profile = "prod"

[profiles.prod.opensearch]
hosts = ["https://os1.internal:9200", "https://os2.internal:9200"]
username = "admin"
password = "env:DBOPS_OS_PASSWORD"      # env: / cmd: / 리터럴 3종 지원

[profiles.prod.mongodb]
uri = "cmd:vault read -field=uri secret/mongo/prod"

[profiles.prod.postgres]
host = "pg01.internal"
port = 5432
user = "dbops"
password = "env:DBOPS_PG_PASSWORD"
dbname = "app"

[profiles.prod.redis]
uri = "env:DBOPS_REDIS_URI"

[safety]
protected_profiles = ["prod"]            # 파괴적 커맨드에 --confirm-name 강제
```

시크릿은 config에 평문으로 두지 않는다: `env:VAR_NAME`은 프로세스 환경변수를,
`cmd:커맨드`는 셸 명령의 stdout(5초 타임아웃, trailing newline 제거)을 읽는다. 그 외 값은
리터럴로 취급된다. config 파일 권한이 `0600`이 아니면 실행 시 경고가 출력된다.

### `DBOPS_*` 환경변수

| 변수 | 대상 필드 |
|---|---|
| `DBOPS_PROFILE` | 사용할 프로파일 이름 (`--profile` 플래그보다는 낮고, config 파일의 `default_profile`보다는 높은 우선순위) |
| `DBOPS_OS_HOSTS` | `opensearch.hosts` (콤마로 구분된 여러 호스트) |
| `DBOPS_OS_USERNAME` | `opensearch.username` |
| `DBOPS_OS_PASSWORD` | `opensearch.password` |
| `DBOPS_MONGO_URI` | `mongodb.uri` |
| `DBOPS_PG_HOST` | `postgres.host` |
| `DBOPS_PG_PORT` | `postgres.port` |
| `DBOPS_PG_USER` | `postgres.user` |
| `DBOPS_PG_PASSWORD` | `postgres.password` |
| `DBOPS_PG_DBNAME` | `postgres.dbname` |
| `DBOPS_REDIS_URI` | `redis.uri` |

별도로, `mongo init user --password`는 셸 히스토리/`ps` 노출을 피하기 위해
`DBOPS_NEW_USER_PASSWORD` 환경변수를 우선 권장한다(프로파일 필드가 아니라 이 서브커맨드
전용 값).

## exit code 규약

`health` 커맨드는 nagios/check_postgres 규약을 그대로 따른다 — 이 매핑은 릴리스 간 절대
바뀌지 않는 계약이다:

| exit | 의미 |
|---|---|
| 0 | OK |
| 1 | WARNING |
| 2 | CRITICAL |
| 3 | UNKNOWN (접속 불가, 타임아웃, `--warning`/`--critical` 값 오류 등) |

그 외 커맨드(`init`/`reset`/`seed`, 조회 커맨드의 접속 실패 등)는 별도의 unix 관례를 쓴다:

| exit | 의미 |
|---|---|
| 0 | 성공 |
| 1 | 일반 오류 |
| 2 | 확인 거부 (`--yes` 없는 non-TTY, 또는 `--confirm-name` 불일치) |
| 3 | 인자 오류 |
| 4 | 접속 실패 |

### 모니터링 연동 예시

nrpe/nagios 플러그인처럼 그대로 등록 가능:

```bash
# NRPE 커맨드 정의 (nagios 서버 쪽)
command[check_pg_prod]=/usr/local/bin/dbops pg health --profile prod --warning 5s --critical 30s
```

cron + 메일 알림 래퍼 예시:

```bash
#!/bin/sh
# /etc/cron.d 에서 5분마다 실행
dbops redis health --profile prod --warning 100ms --critical 500ms
code=$?
if [ "$code" -ge 1 ]; then
  echo "redis health exit=$code" | mail -s "redis health degraded" oncall@example.com
fi
exit "$code"
```

## 파괴적 커맨드 안전장치

`init`/`reset`/`seed`류는 전부 동일한 3중 가드(`frame::guard::authorize`)를 통과해야 실제로
실행된다. 판정 순서(첫 매치 승):

1. **`--dry-run`** — 계획만 출력하고 exit 0. 실제 실행과 정확히 같은 plan을 그리기 때문에
   dry-run 결과와 실행 결과가 어긋나지 않는다.
2. **non-TTY(스크립트/cron) + `--yes` 없음** — 무조건 거부, exit 2. 자동화 스크립트가
   실수로 파괴적 커맨드를 실행하는 걸 막는다.
3. **보호된 프로파일**(`[safety] protected_profiles`) — `--confirm-name <대상이름>`이
   정확히 일치해야 통과. TTY라면 이름을 다시 입력하라는 프롬프트가 뜨고, non-TTY라면 즉시
   거부(exit 2).
4. **TTY + `--yes` 없음** — 마지막 "정말 실행?" 확인 프롬프트.
5. 위 전부 통과 시에만 실제 적용.

즉 CI/자동화에서 돌리려면 `--yes`가 필수고, `prod`처럼 보호된 프로파일이면 `--yes`가
있어도 `--confirm-name`까지 정확히 맞아야 한다.

## Day-0 데모 (3분)

```bash
# 1. 배포 (의존성 없음 증명)
scp dist/dbops-<version>-x86_64-unknown-linux-musl pg01:/usr/local/bin/dbops
ssh pg01 'ldd /usr/local/bin/dbops; dbops --version'

# 2. 4종 점검
dbops pg health && dbops mongo health && dbops os health && dbops redis health

# 3. 통계
dbops os indices; dbops mongo stats --db app; dbops pg tables --top 5

# 4. 안전장치 시연 (아무것도 안 지워짐)
dbops os reset index demo-idx --dry-run

# 5. 모니터링 연동 증명
dbops pg health --critical 1ms; echo "exit=$?"   # → 2
```

## 알려진 제약

- **OpenSearch `https://` + `--insecure` 조합 미지원.** `opensearch` crate가
  `native-tls`/`rustls-tls` 둘 다 비활성화된 채 빌드된다 — 두 feature 모두 `reqwest`의
  `rustls` feature로 이어지고, 이는 이 프로젝트가 musl 크로스 빌드 회귀 때문에 전역적으로
  피하는 `aws-lc-rs` 크립토 백엔드를 강제한다(`ring`으로 고정). 그 결과 인증서 검증을 끄는
  코드 경로 자체가 바이너리에 존재하지 않는다. `http://` 호스트나 유효한 인증서를 쓰는
  `https://` 호스트는 정상 동작한다. 자세한 내용은
  [`docs/build-spike.md`](docs/build-spike.md)와 `src/os/client.rs`의 모듈 문서 참고.
- **`pg`에는 별도 `seed` 서브커맨드가 없다** (의도된 설계). 시드 데이터는
  `pg init schema --file`에 넘기는 SQL 파일에 `INSERT` 문을 함께 넣어 커버한다 —
  `os`/`mongo`는 NDJSON bulk insert 전용 `seed` 커맨드가 있지만, pg는 이미 임의 SQL을
  트랜잭션으로 적용하는 `init schema`가 있어 별도 커맨드를 만들지 않았다.
- **`redis slowlog`의 `duration_us` 컬럼 단위는 마이크로초다** — `pg`/`redis health`의 다른
  시간 필드들이 밀리초인 것과 다르므로 컬럼명이 단위를 명시한다. 착각하지 않도록
  테이블/`--json` 양쪽에 `duration`이 아니라 `duration_us`로 표기된다.

## Shell completion

```bash
# bash (예: 시스템 전역)
dbops completion bash | sudo tee /etc/bash_completion.d/dbops

# zsh
dbops completion zsh > "${fpath[1]}/_dbops"

# fish
dbops completion fish > ~/.config/fish/completions/dbops.fish
```

`powershell`/`elvish`도 지원한다. completion 생성은 config/프로파일 해석을 전혀 거치지
않으므로 `~/.dbops.toml`이 없거나 깨져 있어도 항상 동작한다.

## 테스트

- **`bash tests/integration.sh`** — docker compose로 pg(primary+replica)/mongo(3노드
  replset)/opensearch/redis를 띄우고 모든 `health`/조회 커맨드를 실제 바이너리로
  검증한다 (`--keep`으로 뒤처리 없이 유지 가능).
- **`bash tests/destructive_matrix.sh`** — `init`/`reset`/`seed`의 3중 가드(dry-run /
  non-TTY 거부 / protected profile 이름 확인)를 실제 DB 상태 변경 여부까지 교차검증한다.

둘 다 자체 docker compose 프로젝트(포트 대역도 분리)라 동시에 실행 가능하고,
CI(`.github/workflows/ci.yml`)에서 별도 job으로 병렬 실행된다. 요구사항: Docker
(`docker compose` v2), `jq`, `cargo`. 자세한 내용은 [`tests/README.md`](tests/README.md).

## 빌드

```bash
cargo build --release                 # 로컬 호스트 타깃
scripts/release-build.sh              # 호스트 + 2개 musl 타깃, 정적 링크/크립토 백엔드
                                      # 게이트, dist/ 패키징까지 한 번에
```

release 프로파일(`lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `opt-level = "z"`,
`strip = true`)은 `strip`만 켰을 때보다 바이너리를 약 60~70% 줄인다 — 자세한 수치는
[`docs/build-spike.md`](docs/build-spike.md) 참고. 태그(`vX.Y.Z`)를 push하면
`.github/workflows/release.yml`이 3타깃을 각각 빌드해 GitHub Release에 첨부한다.
