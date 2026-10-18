from typing import Annotated

from odoo import api, models


class RecordVars(models.Model):
    _name = "record.vars"
    _description = "Record Vars"

    def is_open(self):
        return True

    def in_generator(self):
        return any(rec.is_open() for rec in self)

    def in_list_comp(self):
        return [rec for rec in self.search([]) if rec.is_open()]

    def in_dict_comp(self):
        return {rec.id: rec.is_open() for rec in self}

    def in_nested_comp(self):
        return [line for rec in self for line in rec if line.is_open()]

    def in_filtered(self):
        return self.filtered(lambda rec: rec.is_open())

    def in_sorted(self):
        return self.sorted(key=lambda rec: rec.is_open())

    def in_chain(self):
        return self.filtered(lambda rec: rec.is_open()).mapped("id")

    def in_union(self, other):
        return (self | other).filtered(lambda rec: rec.is_open())

    def shadowing(self):
        rec = 1
        names = [rec for rec in self]
        return rec, names


def in_router(env: Annotated[api.Environment, "a dependency"]):
    return env["record.vars"].search([])


class RecordVarsAssign(models.Model):
    _inherit = "record.vars"

    def reassigned(self):
        records = self.search([])
        records = records.filtered(lambda rec: rec.is_open())
        lines = [line for line in records if line.is_open()]
        return records.mapped(lambda rec: [sub for sub in rec if sub.is_open()]), lines

    def completion_probe(self):
        values = [rec.id for rec in self if rec]
        return self.browse({"id": values})
