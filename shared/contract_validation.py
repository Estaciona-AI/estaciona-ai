# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Test-only validation of contract bundles; references never retrieve remote data."""

from jsonschema import Draft202012Validator, FormatChecker
from referencing import Registry, Resource


class ContractValidator:
    def __init__(self, documents):
        self.documents = dict(documents)
        for document in self.documents.values():
            Draft202012Validator.check_schema(document)
        self.registry = Registry().with_resources(
            (document["$id"], Resource.from_contents(document))
            for document in self.documents.values()
        )

    def validator(self, path, *, collection=False):
        document = self.documents[path]
        schema = (
            {"type": "array", "items": {"$ref": document["$id"]}}
            if collection
            else document
        )
        return Draft202012Validator(
            schema, registry=self.registry, format_checker=FormatChecker()
        )

    def validate(self, path, instance, *, collection=False):
        self.validator(path, collection=collection).validate(instance)
