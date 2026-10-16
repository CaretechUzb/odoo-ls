from odoo import fields, models


def helper_function(value):
    return value


class GraphBase:
    def base_method(self):
        return helper_function(1)


class GraphChild(GraphBase):
    def child_method(self):
        return self.base_method()


class GraphModel(models.Model):
    _name = "graph.model"
    _description = "Graph Model"

    name = fields.Char()

    def action_done(self):
        return True

    def action_feedback(self):
        self.action_done()
        return helper_function(self.name)

    def action_feedback_schedule_next(self):
        return self.action_done()

    @property
    def label(self):
        return self.name

    @label.setter
    def label(self, value):
        self.name = value


class GraphCaller(models.Model):
    _name = "graph.caller"
    _description = "Graph Caller"

    def call_through_env(self):
        return self.env["graph.model"].action_done()
