#!/bin/sh
# Invoked only after the signed manifest, archive digest and staged code signature pass.
# Finite, unprivileged helper; it never starts providers or leaves a service behind.
parent_pid=$1
destination=$2
staging=$3
receipt=$4
restart=$5
installed_version=$6
backup="$staging/previous.app"
result=failed
count=0
while kill -0 "$parent_pid" 2>/dev/null; do
  count=$((count + 1))
  [ "$count" -le 30 ] || break
  /bin/sleep 1
done
if ! kill -0 "$parent_pid" 2>/dev/null &&
   [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$destination/Contents/Info.plist" 2>/dev/null)" = local.workflow.desktop ] &&
   [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$destination/Contents/Info.plist" 2>/dev/null)" = "$installed_version" ] &&
   /usr/bin/codesign --verify --deep --strict --all-architectures "$staging/Workflow.app" 2>/dev/null; then
  if /bin/mv "$destination" "$backup"; then
    if /bin/mv "$staging/Workflow.app" "$destination"; then
      result=installed
    else
      /bin/mv "$backup" "$destination" || result=rollback-failed
    fi
  fi
fi
printf '%s' "$result" > "$receipt"
if [ "$result" = installed ]; then
  /bin/rm -rf "$backup"
  if [ "$restart" = 1 ]; then
    /usr/bin/open -a "$destination" || printf '%s' restart-failed > "$receipt"
  fi
fi
# Preserve the backup for recovery if a rollback failed.
if [ ! -e "$backup" ]; then /bin/rm -rf "$staging"; fi
