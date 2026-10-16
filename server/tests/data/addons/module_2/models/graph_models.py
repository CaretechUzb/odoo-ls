from odoo import models


class GraphModel(models.Model):
    _inherit = "graph.model"

    def action_done(self):
        res = super().action_done()
        return res


class GraphMixinUser(models.Model):
    _name = "graph.mixin.user"
    _inherit = ["graph.model"]
    _description = "Graph Mixin User"


from odoo.addons.module_1.models.graph_models import helper_function


def module_2_helper_user():
    return helper_function(2)
