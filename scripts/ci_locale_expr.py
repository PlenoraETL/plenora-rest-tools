"""Sottoinsieme esplicito delle espressioni Actions, senza eval o shell.

Le espressioni ignote fermano il passo: non sono mai trattate come vere.
"""
from __future__ import annotations

import re


class Unsupported(ValueError):
    """Semantica che il runner locale non sa riprodurre."""


TOKEN = re.compile(r"\s*(==|!=|&&|\|\||[!()]|'(?:[^']|'')*'|[\w.-]+)")
EXPR = re.compile(r"\$\{\{(.*?)\}\}", re.S)


def truth(value):
    return value is not None and value is not False and value != "" and value != 0


def evaluate(source, context):
    source = str(source).strip()
    if source.startswith("${{") and source.endswith("}}"):
        source = source[3:-2].strip()
    tokens, position = [], 0
    while position < len(source):
        match = TOKEN.match(source, position)
        if not match:
            raise Unsupported(f"espressione non supportata: {source}")
        tokens.append(match[1])
        position = match.end()
    index = 0

    def atom():
        nonlocal index
        if index >= len(tokens):
            raise Unsupported(f"espressione incompleta: {source}")
        token = tokens[index]
        index += 1
        if token == "!":
            return not truth(atom())
        if token == "(":
            value = parse(0)
            if index >= len(tokens) or tokens[index] != ")":
                raise Unsupported("parentesi non bilanciate")
            index += 1
            return value
        if token.startswith("'"):
            return token[1:-1].replace("''", "'")
        if token in ("true", "false", "null"):
            return {"true": True, "false": False, "null": None}[token]
        if token.isdigit():
            return int(token)
        if index + 1 < len(tokens) and tokens[index:index + 2] == ["(", ")"]:
            index += 2
            token += "()"
        if token not in context:
            raise Unsupported(f"contesto non disponibile: {token}")
        return context[token]

    def parse(level):
        nonlocal index
        if level == 3:
            return atom()
        operators = [("||",), ("&&",), ("==", "!=")][level]
        left = parse(level + 1)
        while index < len(tokens) and tokens[index] in operators:
            op = tokens[index]
            index += 1
            right = parse(level + 1)
            if op == "||":
                left = left if truth(left) else right
            elif op == "&&":
                left = right if truth(left) else left
            else:
                if type(left) is not type(right):
                    raise Unsupported("confronto fra tipi diversi non supportato")
                equal = left.lower() == right.lower() if isinstance(left, str) else left == right
                left = equal == (op == "==")
        return left

    value = parse(0)
    if index != len(tokens):
        raise Unsupported(f"espressione non supportata: {source}")
    return value


def render(value, context):
    if isinstance(value, dict):
        return {key: render(child, context) for key, child in value.items()}
    if isinstance(value, list):
        return [render(child, context) for child in value]
    if not isinstance(value, str):
        return value
    def replace(match):
        result = evaluate(match[1], context)
        return str(result).lower() if isinstance(result, bool) else str(result)
    return EXPR.sub(replace, value)


def partial(value, context):
    """Il piano conserva le espressioni runtime non ancora risolvibili."""
    if isinstance(value, dict):
        return {key: partial(child, context) for key, child in value.items()}
    if isinstance(value, list):
        return [partial(child, context) for child in value]
    try:
        return render(value, context)
    except Unsupported:
        return value
