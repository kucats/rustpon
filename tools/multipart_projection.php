<?php
// Captures real PHP SAPI multipart registration. Synthetic test input only.
declare(strict_types=1);
function tagged(mixed $value): array {
    if (is_array($value)) {
        $entries = [];
        foreach ($value as $key => $child) {
            $tag = is_int($key) ? ['i', (string) $key] : ['s', bin2hex($key)];
            $entries[] = [$tag, tagged($child)];
        }
        return ['a', $entries];
    }
    if (is_int($value)) { return ['i', (string)$value]; }
    if (!is_string($value)) { throw new RuntimeException('Unexpected type'); }
    return ['s', bin2hex($value)];
}
// Stable metadata-only view: int leaves are ASCII strings and server-side
// temporary paths are replaced with a sentinel, including nested arrays.
function normalized_files(
    mixed $value,
    int $depth = 0,
    bool $isTmpName = false,
    bool $preserveIntegers = false
): mixed {
    if (is_array($value)) {
        foreach ($value as $key => $child) {
            // tmp_name is an attribute in the second level only; an uploaded
            // field can independently have a nested key literally tmp_name.
            $isTmpColumn = $depth === 1 && $key === 'tmp_name';
            $value[$key] = normalized_files(
                $child, $depth + 1, $isTmpName || $isTmpColumn, $preserveIntegers
            );
        }
        return $value;
    }
    if ($isTmpName) {
        return $value === '' ? '' : '__RUSTPON_STAGED__';
    }
    if ($preserveIntegers && is_int($value)) {
        return $value;
    }
    return (string)$value;
}
$files = [];
$uploadModes = [];
foreach ($_FILES as $name => $info) {
    // The current corpus uses flat, distinct file-field names. Nested
    // $_FILES shapes are a separate compatibility milestone.
    if (!is_string($info['name']) || $info['error'] !== UPLOAD_ERR_OK) {
        continue;
    }
    $data = file_get_contents($info['tmp_name']);
    if ($data === false) { throw new RuntimeException('Cannot read upload'); }
    $mode = fileperms($info['tmp_name']);
    if ($mode === false) { throw new RuntimeException('Cannot stat upload'); }
    $uploadModes[] = $mode & 07777;
    // Never disclose server-side temporary paths or host-specific metadata.
    $files[] = [
        bin2hex((string)$name),
        bin2hex($info['name']),
        bin2hex($info['type']),
        strlen($data),
        bin2hex($data),
    ];
}
header('Content-Type: application/json');
echo json_encode(['post_tree' => tagged($_POST), 'files' => $files,
                  'upload_modes' => $uploadModes,
                  'files_tree' => tagged(normalized_files($_FILES)),
                  'files_tree_typed' => tagged(normalized_files($_FILES, 0, false, true))],
                 JSON_THROW_ON_ERROR), "\n";
