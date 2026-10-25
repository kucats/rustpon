<?php
// Synthetic binary-safe PHP parse_str() differential oracle.
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
$body = file_get_contents('php://stdin');
if ($body === false) { fwrite(STDERR, "Failed to read stdin\n"); exit(1); }
parse_str($body, $post);
echo json_encode(tagged($post), JSON_THROW_ON_ERROR), "\n";
