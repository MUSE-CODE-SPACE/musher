#!/bin/sh
# Builds a throwaway repo and runs the demo plan in it.
set -e
here=$(cd "$(dirname "$0")" && pwd)
musher=${MUSHER:-musher}
command -v "$musher" >/dev/null 2>&1 || musher="$here/../../target/release/musher"
[ -x "$musher" ] || { echo "build first: cargo build --release"; exit 1; }

demo=$(mktemp -d "${TMPDIR:-/tmp}/musher-demo.XXXXXX")
cd "$demo"
git init -q
cp "$here/musher.toml" .
git add musher.toml
git -c user.name=demo -c user.email=demo@localhost commit -qm "demo plan"
echo "demo repo: $demo"
echo

"$musher" run --dry-run
echo
"$musher" run || true

echo
echo "deploy failed on purpose. Fix it in the plan and resume:"
sed -i.bak 's/test -f FIXED || .*; echo shipped/echo shipped/' musher.toml && rm -f musher.toml.bak
git -c user.name=demo -c user.email=demo@localhost commit -qam "fix deploy"
"$musher" resume

echo
"$musher" merge
echo
ls
