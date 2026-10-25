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
header('Content-Type: application/json');
echo json_encode(['post_tree' => tagged($_POST)], JSON_THROW_ON_ERROR), "\n";
