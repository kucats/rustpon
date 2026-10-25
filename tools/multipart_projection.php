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
    if (!is_string($value)) { throw new RuntimeException('Unexpected type'); }
    return ['s', bin2hex($value)];
}
// Stable metadata-only view: int leaves are ASCII strings and server-side
// temporary paths are replaced with a sentinel, including nested arrays.
function normalized_files(mixed $value, bool $isTmpName = false): mixed {
    if (is_array($value)) {
        foreach ($value as $key => $child) {
            $value[$key] = normalized_files($child, $isTmpName || $key === 'tmp_name');
        }
        return $value;
    }
    if ($isTmpName) {
        return $value === '' ? '' : '__RUSTPON_STAGED__';
    }
    return (string)$value;
}
$files = [];
foreach ($_FILES as $name => $info) {
    // The current corpus uses flat, distinct file-field names. Nested
    // $_FILES shapes are a separate compatibility milestone.
    if (!is_string($info['name']) || $info['error'] !== UPLOAD_ERR_OK) {
        continue;
    }
    $data = file_get_contents($info['tmp_name']);
    if ($data === false) { throw new RuntimeException('Cannot read upload'); }
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
                  'files_tree' => tagged(normalized_files($_FILES))],
                 JSON_THROW_ON_ERROR), "\n";
